//! Platform-aware mount engine: materializes knowledge bases inside a
//! workspace at `.nomi/knowledge/{link_name}` using NTFS junctions on
//! Windows (no privilege required), symlinks on Unix, and a recursive copy
//! is intentionally not used as a fallback: a detached copy would look
//! writable to the agent while silently diverging from the real knowledge
//! base.
//!
//! The mount directory is wholly owned by this module: anything inside it
//! that is not in the desired set (or in [`MANAGED_KEEP`]) gets removed on
//! the next sync. Targets are never touched — removal only deletes the link.
//! Sibling `.nomi/` trees (`.nomi/skills`, …) are
//! never touched either.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};

use crate::KB_MOUNT_REL_DIR;
use crate::workspace_binding::WorkspaceBindingLease;
use nomifun_common::AppError;
use tokio::sync::Mutex as AsyncMutex;

/// One desired mount: `{workspace}/.nomi/knowledge/{link_name}` → `target`.
#[derive(Debug, Clone)]
pub struct MountSpec {
    pub link_name: String,
    pub target: PathBuf,
}

#[derive(Debug, Clone)]
struct ResolvedMountSpec {
    link_name: String,
    target: PathBuf,
}

/// Platform-managed companion files inside the mount root (the self-ignore,
/// the terminal-facing README) — exempt from the stale-entry sweep, and
/// reserved against base link names (see `service::unique_link_name`).
pub(crate) const MANAGED_KEEP: &[&str] = &[".gitignore", "README.md"];

type WorkspaceSyncLock = AsyncMutex<()>;

/// The mount sync is a two-pass reconcile (sweep, then create). Conversation
/// lifecycle locks cannot protect it because multiple conversations may share
/// one custom workspace, so the filesystem transaction needs its own
/// workspace-keyed lock. Weak values keep the process-global registry bounded.
fn workspace_sync_lock(workspace: &Path) -> Arc<WorkspaceSyncLock> {
    static LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Weak<WorkspaceSyncLock>>>> = OnceLock::new();

    let key = workspace_lock_key(workspace);
    let registry = LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut locks = registry.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }

    let lock = Arc::new(AsyncMutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

fn workspace_lock_key(workspace: &Path) -> PathBuf {
    if let Ok(key) = crate::workspace_binding::canonical_workspace_key(workspace) {
        return key;
    }

    // Legacy best-effort mount callers may still invoke sync before a
    // workspace exists. Strict PreparedMountPlan activation has already
    // canonicalized and therefore always takes the shared authority identity
    // above.
    let resolved = std::fs::canonicalize(workspace).unwrap_or_else(|_| {
        if workspace.is_absolute() {
            workspace.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(workspace))
                .unwrap_or_else(|_| workspace.to_path_buf())
        }
    });

    // Windows and default macOS volumes are case-insensitive. Canonicalization
    // normally returns a stable spelling, but it does not promise case
    // normalization for every filesystem/provider, so normalize explicitly.
    // On a case-sensitive APFS volume this only over-serializes two unrelated
    // case variants; it cannot merge their filesystem contents.
    platform_comparison_path(resolved)
}

#[cfg(any(windows, target_os = "macos"))]
fn platform_comparison_path(path: PathBuf) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn platform_comparison_path(path: PathBuf) -> PathBuf {
    path
}

/// Synchronize the workspace mount directory to exactly `specs`.
///
/// Returns the link names that are present as live links after the
/// sync. Individual failures are logged and skipped — mounting must never
/// brick a session start.
pub async fn sync_mounts(workspace: &Path, specs: Vec<MountSpec>) -> Vec<String> {
    sync_mounts_transaction(workspace, specs, None)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "knowledge mount task join error");
            Vec::new()
        })
}

/// Strict mount reconciliation under an exact cross-process workspace lease.
///
/// The cloned authority is moved into the blocking transaction together with
/// the process-local sync guard. `spawn_blocking` outlives a cancelled async
/// waiter, so retaining only the guard would allow the OS lease to drop while
/// the filesystem sweep/create still runs. Prepared runtime activation must
/// use this path and receives a join failure instead of silently degrading.
pub(crate) async fn sync_mounts_with_authority(
    workspace: &Path,
    specs: Vec<MountSpec>,
    authority: WorkspaceBindingLease,
) -> Result<Vec<String>, AppError> {
    if !authority.matches_workspace(workspace)? {
        return Err(AppError::Conflict(format!(
            "knowledge workspace lease for {} does not protect mount target {}",
            authority.workspace_key().display(),
            workspace.display()
        )));
    }
    sync_mounts_transaction(workspace, specs, Some(authority))
        .await
        .map_err(|error| {
            AppError::Internal(format!(
                "strict knowledge mount transaction failed to join: {error}"
            ))
        })
}

async fn sync_mounts_transaction(
    workspace: &Path,
    specs: Vec<MountSpec>,
    authority: Option<WorkspaceBindingLease>,
) -> Result<Vec<String>, tokio::task::JoinError> {
    let workspace = workspace.to_path_buf();
    let sync_lock = workspace_sync_lock(&workspace);
    let sync_guard = sync_lock.lock_owned().await;
    tokio::task::spawn_blocking(move || {
        // `spawn_blocking` keeps running if the async caller is cancelled.
        // Move both layers of authority into this closure so neither an
        // in-process replacement nor another backend process can overlap the
        // still-running sweep/create transaction.
        let _sync_guard = sync_guard;
        let _authority = authority;
        #[cfg(test)]
        pause_blocking_sync_for_test(&workspace);
        sync_mounts_blocking(&workspace, &specs)
    })
    .await
}

/// Safely publish the terminal-facing knowledge contract after mount sync.
///
/// This deliberately reuses the same physical-workspace lock as [`sync_mounts`]
/// and never creates the mount scaffolding. The destination is replaced by a
/// staged ordinary file, so a pre-existing symlink, junction, reparse point,
/// or hardlink is removed/replaced as an entry and is never opened for write.
pub async fn write_terminal_readme(workspace: &Path, contents: &str) -> io::Result<()> {
    let workspace = workspace.to_path_buf();
    let contents = contents.as_bytes().to_vec();
    let sync_guard = workspace_sync_lock(&workspace).lock_owned().await;
    tokio::task::spawn_blocking(move || {
        // As with mount sync, cancellation of the async caller must not release
        // the lock while the blocking filesystem transaction is still alive.
        let _sync_guard = sync_guard;
        write_terminal_readme_inner(&workspace, &contents)
    })
    .await
    .map_err(io::Error::other)?
}

fn write_terminal_readme_inner(workspace: &Path, contents: &[u8]) -> io::Result<()> {
    let mount_root = prepare_mount_root(workspace, false)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "knowledge mount root does not exist",
        )
    })?;
    validate_mount_root(&mount_root)?;
    let destination = mount_root.join("README.md");
    let staging = mount_root.join(format!(
        ".nomi-managed-readme-{}",
        nomifun_common::generate_id()
    ));

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)?;
    let _staging_cleanup = StagedEntryCleanup(staging.clone());
    std::io::Write::write_all(&mut file, contents)?;
    file.sync_all()?;
    drop(file);
    if !matches!(classify_path(&staging), Ok(PathKind::File))
        || metadata_has_multiple_links(&staging)
    {
        let _ = remove_mount_entry_inner(&staging);
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "knowledge README staging file identity changed",
        ));
    }

    // Remove only link-like or multiply-linked destinations up front.
    // Ordinary files stay in place for the OS atomic replace primitive.
    match std::fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata_is_link_like(&metadata) => {
            remove_link_entry(&destination, &metadata)?;
        }
        Ok(metadata) if metadata.file_type().is_file() => {
            if metadata_has_multiple_links(&destination) {
                std::fs::remove_file(&destination)?;
            }
        }
        Ok(_) => {
            let _ = remove_mount_entry_inner(&staging);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "knowledge README destination is not a regular file",
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            let _ = remove_mount_entry_inner(&staging);
            return Err(e);
        }
    }

    validate_mount_root(&mount_root)?;
    if let Err(e) = atomic_replace_file(&staging, &destination) {
        let _ = remove_mount_entry_inner(&staging);
        return Err(e);
    }
    validate_mount_root(&mount_root)?;
    if !matches!(classify_path(&destination), Ok(PathKind::File))
        || metadata_has_multiple_links(&destination)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "knowledge README destination identity changed",
        ));
    }
    Ok(())
}

struct StagedEntryCleanup(PathBuf);

impl Drop for StagedEntryCleanup {
    fn drop(&mut self) {
        let _ = remove_mount_entry_inner(&self.0);
    }
}

#[cfg(unix)]
fn atomic_replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn atomic_replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let mut source_wide = source.as_os_str().encode_wide().collect::<Vec<_>>();
    source_wide.push(0);
    let mut destination_wide = destination.as_os_str().encode_wide().collect::<Vec<_>>();
    destination_wide.push(0);
    // SAFETY: both buffers are NUL-terminated and remain alive for the call.
    let succeeded = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[derive(Debug)]
struct BlockingSyncTestPause {
    workspace_key: PathBuf,
    entered: StdMutex<Option<tokio::sync::oneshot::Sender<()>>>,
    released: StdMutex<bool>,
    release_signal: std::sync::Condvar,
}

#[cfg(test)]
fn blocking_sync_test_pause_slot() -> &'static StdMutex<Option<Arc<BlockingSyncTestPause>>> {
    static PAUSE: OnceLock<StdMutex<Option<Arc<BlockingSyncTestPause>>>> = OnceLock::new();
    PAUSE.get_or_init(|| StdMutex::new(None))
}

#[cfg(test)]
fn pause_blocking_sync_for_test(workspace: &Path) {
    let pause = blocking_sync_test_pause_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .filter(|pause| pause.workspace_key == workspace_lock_key(workspace))
        .cloned();
    let Some(pause) = pause else {
        return;
    };

    if let Some(entered) = pause
        .entered
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        let _ = entered.send(());
    }
    let mut released = pause.released.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    while !*released {
        released = pause
            .release_signal
            .wait(released)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

fn sync_mounts_blocking(workspace: &Path, specs: &[MountSpec]) -> Vec<String> {
    sync_mounts_inner(workspace, specs)
}

fn sync_mounts_inner(workspace: &Path, specs: &[MountSpec]) -> Vec<String> {
    if specs.is_empty() {
        clear_mount_root(workspace);
        return Vec::new();
    }

    let mount_root = match prepare_mount_root(workspace, true) {
        Ok(Some(root)) => root,
        Ok(None) => return Vec::new(),
        Err(e) => {
            tracing::warn!(
                workspace = %workspace.display(),
                error = %e,
                "refusing unsafe knowledge mount path"
            );
            return Vec::new();
        }
    };

    // Self-ignore the mount directory: when the workspace is a user git
    // repo, junctions would otherwise expose the knowledge base content as
    // committable project files. The ignore file lives INSIDE
    // `.nomi/knowledge/` — never at the `.nomi/` root — so committable
    // siblings like `.nomi/skills` stay visible to git.
    let gitignore = mount_root.join(".gitignore");
    if let Err(e) = ensure_managed_gitignore(&mount_root, &gitignore) {
        tracing::warn!(
            path = %gitignore.display(),
            error = %e,
            "refusing unsafe knowledge mount .gitignore"
        );
        return Vec::new();
    }

    // Filter at this trust boundary even though the service currently creates
    // link names. A path separator, `..`, Windows ADS (`:`), or a managed name
    // must never turn a mount spec into an operation outside `mount_root`.
    let mut desired = HashMap::<String, usize>::new();
    let mut desired_order = Vec::<ResolvedMountSpec>::new();
    for spec in specs {
        if !is_safe_link_name(&spec.link_name) {
            tracing::warn!(name = %spec.link_name, "rejecting unsafe knowledge mount name");
            continue;
        }
        let target = match resolve_mount_target(&mount_root, &spec.target) {
            Ok(target) => target,
            Err(e) => {
                tracing::warn!(
                    name = %spec.link_name,
                    target = %spec.target.display(),
                    error = %e,
                    "knowledge base root is unsafe or missing; skipping mount"
                );
                continue;
            }
        };
        let name_key = mount_name_key(&spec.link_name);
        if desired.contains_key(&name_key) {
            tracing::warn!(name = %spec.link_name, "ignoring duplicate knowledge mount name");
            continue;
        }
        desired.insert(name_key, desired_order.len());
        desired_order.push(ResolvedMountSpec {
            link_name: spec.link_name.clone(),
            target,
        });
    }

    // Pass 1: drop stale entries and stale links whose target changed. Plain
    // directories are legacy copy fallbacks and must be removed: reporting a
    // detached writable copy as mounted would silently diverge from the real
    // knowledge base.
    if let Err(e) = validate_mount_root(&mount_root) {
        tracing::warn!(path = %mount_root.display(), error = %e, "knowledge mount root changed during sync");
        return Vec::new();
    }
    let entries = match std::fs::read_dir(&mount_root) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(path = %mount_root.display(), error = %e, "failed to read knowledge mount dir");
            return Vec::new();
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!(path = %mount_root.display(), error = %e, "failed to enumerate knowledge mount dir");
                return Vec::new();
            }
        };
        if let Err(e) = validate_mount_root(&mount_root) {
            tracing::warn!(path = %mount_root.display(), error = %e, "knowledge mount root changed during stale sweep");
            return Vec::new();
        }

        let path = entry.path();
        let file_name = entry.file_name();
        if let Some(canonical_name) = managed_canonical_name(&file_name) {
            if file_name != OsStr::new(canonical_name)
                || !matches!(classify_path(&path), Ok(PathKind::File))
                || metadata_has_multiple_links(&path)
            {
                // In particular, never leave a hostile README symlink/reparse
                // point or hardlink for the terminal service to overwrite.
                remove_mount_entry(&path);
            }
            continue;
        }
        let name = file_name.to_string_lossy();
        match desired.get(&mount_name_key(&name)) {
            None => remove_mount_entry(&path),
            Some(spec_index) => {
                let spec = &desired_order[*spec_index];
                match classify_path(&path) {
                    Ok(PathKind::LinkLike) => {
                        if !link_target_matches(&path, &spec.target) {
                            remove_mount_entry(&path);
                        }
                    }
                    Ok(PathKind::Directory) => remove_mount_entry(&path),
                    Ok(PathKind::Missing) => {}
                    Ok(PathKind::File | PathKind::Other) | Err(_) => remove_mount_entry(&path),
                }
            }
        }
    }

    // Pass 2: create whatever is missing.
    let mut present = Vec::new();
    for spec in &desired_order {
        if let Err(e) = validate_mount_root(&mount_root) {
            tracing::warn!(path = %mount_root.display(), error = %e, "knowledge mount root changed during create pass");
            break;
        }

        let link = mount_root.join(&spec.link_name);
        match classify_path(&link) {
            Ok(PathKind::LinkLike) if link_target_matches(&link, &spec.target) => {
                present.push(spec.link_name.clone());
                continue;
            }
            Ok(PathKind::Missing) => {}
            Ok(_) | Err(_) => {
                remove_mount_entry(&link);
                if !matches!(classify_path(&link), Ok(PathKind::Missing)) {
                    continue;
                }
            }
        }

        // The target may have disappeared since the first pass. Do not create
        // a broken alias in that case.
        if let Err(e) = resolve_mount_target(&mount_root, &spec.target) {
            tracing::warn!(
                target = %spec.target.display(),
                name = %spec.link_name,
                error = %e,
                "knowledge base root changed during sync; skipping mount"
            );
            continue;
        }
        match create_link(&spec.target, &link) {
            Ok(()) if link_target_matches(&link, &spec.target) => {
                present.push(spec.link_name.clone());
            }
            Ok(()) => {
                tracing::warn!(
                    target = %spec.target.display(),
                    link = %link.display(),
                    "knowledge link target could not be verified; removing mount"
                );
                remove_mount_entry(&link);
            }
            Err(e) => {
                tracing::warn!(
                    target = %spec.target.display(),
                    link = %link.display(),
                    error = %e,
                    raw_os_error = ?e.raw_os_error(),
                    "knowledge link failed; skipping mount to avoid a stale writable copy"
                );
            }
        }
    }
    present
}

fn clear_mount_root(workspace: &Path) {
    let mount_root = match prepare_mount_root(workspace, false) {
        Ok(Some(root)) => root,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(
                workspace = %workspace.display(),
                error = %e,
                "refusing unsafe knowledge mount cleanup path"
            );
            return;
        }
    };

    let entries = match std::fs::read_dir(&mount_root) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(path = %mount_root.display(), error = %e, "failed to read knowledge mount dir for cleanup");
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!(path = %mount_root.display(), error = %e, "failed to enumerate knowledge mount dir for cleanup");
                return;
            }
        };
        if validate_mount_root(&mount_root).is_err() {
            tracing::warn!(path = %mount_root.display(), "knowledge mount root changed during cleanup");
            return;
        }
        remove_mount_entry(&entry.path());
    }

    if validate_mount_root(&mount_root).is_err() {
        return;
    }
    if let Err(e) = std::fs::remove_dir(&mount_root) {
        tracing::debug!(path = %mount_root.display(), error = %e, "knowledge mount dir remains non-empty");
        return;
    }

    // Remove `.nomi/` only when it is still a plain directory and empty.
    if let Some(parent) = mount_root.parent()
        && ensure_plain_directory(parent, false).unwrap_or(false)
    {
        let _ = std::fs::remove_dir(parent);
    }
}

/// Build the managed path one component at a time. `create_dir_all` and
/// `Path::exists` follow aliases, which would let a pre-existing `.nomi` or
/// `.nomi/knowledge` symlink/junction redirect the stale sweep outside the
/// workspace.
fn prepare_mount_root(workspace: &Path, create: bool) -> io::Result<Option<PathBuf>> {
    match std::fs::metadata(workspace) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace is not a directory",
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound && !create => return Ok(None),
        Err(e) => return Err(e),
    }

    let nomi_root = workspace.join(".nomi");
    let mount_root = workspace.join(KB_MOUNT_REL_DIR);
    if mount_root.parent() != Some(nomi_root.as_path()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "knowledge mount constant escapes .nomi",
        ));
    }

    if !ensure_plain_directory(&nomi_root, create)? {
        return Ok(None);
    }
    if !ensure_plain_directory(&mount_root, create)? {
        return Ok(None);
    }
    // Recheck the ancestor after creating/inspecting the child. This rejects
    // deterministic alias replacement and narrows the external-race window.
    ensure_plain_directory(&nomi_root, false)?;
    ensure_plain_directory(&mount_root, false)?;
    Ok(Some(mount_root))
}

fn validate_mount_root(mount_root: &Path) -> io::Result<()> {
    let parent = mount_root.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "knowledge mount root has no parent")
    })?;
    if !ensure_plain_directory(parent, false)? || !ensure_plain_directory(mount_root, false)? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "knowledge mount root disappeared",
        ));
    }
    Ok(())
}

fn ensure_plain_directory(path: &Path, create: bool) -> io::Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound && !create => return Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            match std::fs::create_dir(path) {
                Ok(()) => {}
                Err(create_error) if create_error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(create_error) => return Err(create_error),
            }
            std::fs::symlink_metadata(path)?
        }
        Err(e) => return Err(e),
    };

    if metadata_is_link_like(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing symlink/junction/reparse directory: {}", path.display()),
        ));
    }
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("managed path is not a directory: {}", path.display()),
        ));
    }
    Ok(true)
}

fn ensure_managed_gitignore(mount_root: &Path, path: &Path) -> io::Result<()> {
    validate_mount_root(mount_root)?;
    if path.parent() != Some(mount_root) || path.file_name() != Some(OsStr::new(".gitignore")) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed .gitignore path escapes mount root",
        ));
    }

    let mut aliases = Vec::new();
    for entry in std::fs::read_dir(mount_root)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().eq_ignore_ascii_case(".gitignore") {
            aliases.push(entry.path());
        }
    }

    // Avoid churn only for the exact canonical spelling, a plain file, and
    // the exact required contents. Every other casing/type/content is rebuilt.
    if aliases.len() == 1
        && aliases[0].file_name() == Some(OsStr::new(".gitignore"))
        && matches!(classify_path(&aliases[0]), Ok(PathKind::File))
        && !metadata_has_multiple_links(&aliases[0])
        && std::fs::read(&aliases[0]).is_ok_and(|contents| contents == b"*\n")
    {
        return Ok(());
    }

    let staging = mount_root.join(format!(
        ".nomi-managed-gitignore-{}",
        nomifun_common::generate_id()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)?;
    std::io::Write::write_all(&mut file, b"*\n")?;
    file.sync_all()?;
    drop(file);

    for alias in aliases {
        if let Err(e) = remove_mount_entry_inner(&alias) {
            let _ = remove_mount_entry_inner(&staging);
            return Err(e);
        }
    }
    validate_mount_root(mount_root)?;
    if let Err(e) = std::fs::rename(&staging, path) {
        let _ = remove_mount_entry_inner(&staging);
        return Err(e);
    }
    Ok(())
}

fn is_safe_link_name(name: &str) -> bool {
    if name.is_empty()
        || name.chars().any(|character| matches!(character, '/' | '\\' | ':'))
        || name.ends_with('.')
        || name.ends_with(' ')
        || mount_name_key(name).starts_with(".nomi-copy-")
        || mount_name_key(name).starts_with(".nomi-managed-")
        || MANAGED_KEEP.iter().any(|managed| managed.eq_ignore_ascii_case(name))
        || is_windows_device_name(name)
    {
        return false;
    }
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(component)) if component == OsStr::new(name))
        && components.next().is_none()
}

fn is_windows_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9')
            })
}

fn managed_canonical_name(name: &OsStr) -> Option<&'static str> {
    let name = name.to_string_lossy();
    MANAGED_KEEP
        .iter()
        .copied()
        .find(|managed| managed.eq_ignore_ascii_case(&name))
}

fn metadata_has_multiple_links(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return true;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.nlink() > 1
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, OPEN_EXISTING,
        };

        let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        wide.push(0);
        // SAFETY: `wide` is NUL-terminated and lives through both Win32 calls.
        // The handle is checked before use and closed exactly once.
        unsafe {
            let handle = CreateFileW(
                wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            );
            if handle == INVALID_HANDLE_VALUE {
                return true;
            }
            let mut information = BY_HANDLE_FILE_INFORMATION::default();
            let succeeded = GetFileInformationByHandle(handle, &mut information) != 0;
            let _ = CloseHandle(handle);
            !succeeded || information.nNumberOfLinks > 1
        }
    }
}

fn mount_name_key(name: &str) -> String {
    // Use one conservative identity rule on every platform. This prevents a
    // desired set that works on Linux from collapsing `Docs` and `docs` onto
    // one entry on Windows or a default case-insensitive APFS volume.
    name.to_lowercase()
}

fn resolve_mount_target(mount_root: &Path, target: &Path) -> io::Result<PathBuf> {
    let target_metadata = std::fs::metadata(target)?;
    if !target_metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "knowledge base root is not a directory",
        ));
    }
    let target = std::fs::canonicalize(target)?;
    let mount_root = std::fs::canonicalize(mount_root)?;
    let target_comparison = platform_comparison_path(target.clone());
    let mount_comparison = platform_comparison_path(mount_root);
    if target_comparison.starts_with(&mount_comparison) || mount_comparison.starts_with(&target_comparison) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "knowledge base root overlaps the managed mount tree",
        ));
    }
    Ok(target)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    Missing,
    LinkLike,
    Directory,
    File,
    Other,
}

fn classify_path(path: &Path) -> io::Result<PathKind> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_link_like(&metadata) => Ok(PathKind::LinkLike),
        Ok(metadata) if metadata.file_type().is_dir() => Ok(PathKind::Directory),
        Ok(metadata) if metadata.file_type().is_file() => Ok(PathKind::File),
        Ok(_) => Ok(PathKind::Other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(PathKind::Missing),
        Err(e) => Err(e),
    }
}

#[cfg(windows)]
fn metadata_is_link_like(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_link_like(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// Remove one entry inside the mount dir without ever touching the link
/// target's contents: junctions/symlinks are removed as links; plain
/// directories are legacy copy-fallback leftovers owned by this mount root.
fn remove_mount_entry(path: &Path) {
    if let Err(e) = remove_mount_entry_inner(path) {
        tracing::warn!(path = %path.display(), error = %e, "failed to remove stale knowledge mount entry");
    }
}

fn remove_mount_entry_inner(path: &Path) -> io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if metadata_is_link_like(&metadata) {
        return remove_link_entry(path, &metadata);
    }
    if metadata.file_type().is_dir() {
        // After the no-follow classification above, delegate recursion to the
        // standard library's platform implementation, which removes nested
        // symlinks as entries rather than walking their targets.
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

#[cfg(unix)]
fn remove_link_entry(path: &Path, _metadata: &std::fs::Metadata) -> io::Result<()> {
    std::fs::remove_file(path)
}

#[cfg(windows)]
fn remove_link_entry(path: &Path, metadata: &std::fs::Metadata) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    if metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Resolve the target of a symlink or (on Windows) NTFS junction; `None` for
/// regular files/dirs or when the entry does not exist.
fn read_link_target(path: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if junction::exists(path).unwrap_or(false) {
            return junction::get_target(path).ok();
        }
    }
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() {
        std::fs::read_link(path).ok()
    } else {
        None
    }
}

fn link_target_matches(link: &Path, expected: &Path) -> bool {
    let Some(current) = read_link_target(link) else {
        return false;
    };
    let current = if current.is_absolute() {
        current
    } else {
        link.parent().unwrap_or_else(|| Path::new("")).join(current)
    };
    match (std::fs::canonicalize(&current), std::fs::canonicalize(expected)) {
        (Ok(current), Ok(expected)) => current == expected,
        _ => current == expected,
    }
}

#[cfg(unix)]
fn create_link(src: &Path, dst: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn create_link(src: &Path, dst: &Path) -> io::Result<()> {
    // Junctions work without SeCreateSymbolicLink (Developer Mode/Admin),
    // which most users don't have — mirrors the skill linker's rationale.
    junction::create(src, dst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn make_base(dir: &TempDir, name: &str) -> PathBuf {
        let root = dir.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("note.md"), "# hi").unwrap();
        root
    }

    #[cfg(unix)]
    fn create_directory_alias(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[cfg(windows)]
    fn create_directory_alias(target: &Path, link: &Path) {
        junction::create(target, link).unwrap();
    }

    fn remove_directory_alias(link: &Path) {
        let metadata = std::fs::symlink_metadata(link).unwrap();
        remove_link_entry(link, &metadata).unwrap();
    }

    #[tokio::test]
    async fn mounts_link_and_cleanup() {
        let bases = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let kb_a = make_base(&bases, "kb_a");
        let kb_b = make_base(&bases, "kb_b");

        // Mount both.
        let present = sync_mounts(
            ws.path(),
            vec![
                MountSpec {
                    link_name: "甲".into(),
                    target: kb_a.clone(),
                },
                MountSpec {
                    link_name: "乙".into(),
                    target: kb_b.clone(),
                },
            ],
        )
        .await;
        assert_eq!(present.len(), 2);
        let mount_root = ws.path().join(KB_MOUNT_REL_DIR);
        assert!(mount_root.join("甲").join("note.md").exists());
        assert!(mount_root.join("乙").join("note.md").exists());
        // The mount dir self-ignores so junction content never leaks into
        // the user's git repository.
        let gitignore = mount_root.join(".gitignore");
        assert_eq!(std::fs::read_to_string(&gitignore).unwrap().trim(), "*");

        // Shrink to one — the other must disappear, target stays intact.
        let present = sync_mounts(
            ws.path(),
            vec![MountSpec {
                link_name: "甲".into(),
                target: kb_a.clone(),
            }],
        )
        .await;
        assert_eq!(present, vec!["甲".to_string()]);
        assert!(!mount_root.join("乙").exists());
        assert!(kb_b.join("note.md").exists(), "unmount must not delete target content");

        // Retarget the same name — link must follow.
        let present = sync_mounts(
            ws.path(),
            vec![MountSpec {
                link_name: "甲".into(),
                target: kb_b.clone(),
            }],
        )
        .await;
        assert_eq!(present.len(), 1);
        std::fs::write(kb_b.join("only_b.md"), "b").unwrap();
        assert!(mount_root.join("甲").join("only_b.md").exists());

        // Empty set clears the scaffolding.
        let present = sync_mounts(ws.path(), vec![]).await;
        assert!(present.is_empty());
        assert!(!mount_root.exists());
        assert!(kb_a.join("note.md").exists());
        assert!(kb_b.join("note.md").exists());
    }

    #[tokio::test]
    async fn portable_writeback_missing_target_is_not_reported_as_mounted() {
        let bases = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let target = make_base(&bases, "gone");
        let spec = MountSpec {
            link_name: "Gone".into(),
            target: target.clone(),
        };
        assert_eq!(
            sync_mounts(ws.path(), vec![spec.clone()]).await,
            vec!["Gone"]
        );

        std::fs::remove_dir_all(&target).unwrap();
        let present = sync_mounts(ws.path(), vec![spec]).await;

        assert!(present.is_empty());
        assert!(
            !ws.path().join(KB_MOUNT_REL_DIR).join("Gone").exists()
                && read_link_target(
                    &ws.path().join(KB_MOUNT_REL_DIR).join("Gone")
                )
                .is_none()
        );
    }

    #[tokio::test]
    async fn gitignore_written_inside_knowledge_dir() {
        let bases = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let kb = make_base(&bases, "kb_g");

        sync_mounts(
            ws.path(),
            vec![MountSpec {
                link_name: "甲".into(),
                target: kb,
            }],
        )
        .await;

        // The self-ignore lives INSIDE `.nomi/knowledge/` — pinned to the
        // literal path so a constant regression cannot slip through.
        let inside = ws.path().join(".nomi").join("knowledge").join(".gitignore");
        assert_eq!(std::fs::read_to_string(&inside).unwrap().trim(), "*");
        // Never at the `.nomi/` root: that would shadow committable sibling
        // trees like `.nomi/skills` out of the user's git repository.
        assert!(!ws.path().join(".nomi").join(".gitignore").exists());
    }

    #[tokio::test]
    async fn managed_files_survive_sync() {
        let bases = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let kb = make_base(&bases, "kb_m");
        let spec = || {
            vec![MountSpec {
                link_name: "甲".into(),
                target: kb.clone(),
            }]
        };

        sync_mounts(ws.path(), spec()).await;
        let mount_root = ws.path().join(KB_MOUNT_REL_DIR);
        // Platform-managed companion file (terminal README, see MANAGED_KEEP)
        // must not be swept as a stale mount on the next sync.
        std::fs::write(mount_root.join("README.md"), "# managed").unwrap();

        sync_mounts(ws.path(), spec()).await;
        assert_eq!(
            std::fs::read_to_string(mount_root.join("README.md")).unwrap(),
            "# managed"
        );
        assert_eq!(
            std::fs::read_to_string(mount_root.join(".gitignore")).unwrap().trim(),
            "*"
        );
    }

    #[tokio::test]
    async fn missing_target_is_skipped() {
        let ws = TempDir::new().unwrap();
        let present = sync_mounts(
            ws.path(),
            vec![MountSpec {
                link_name: "ghost".into(),
                target: PathBuf::from("Z:/definitely/not/here"),
            }],
        )
        .await;
        assert!(present.is_empty());
    }

    #[tokio::test]
    async fn portable_writeback_mount_root_link_never_sweeps_external_files() {
        let ws = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let nomi = ws.path().join(".nomi");
        std::fs::create_dir(&nomi).unwrap();
        let mount_root = nomi.join("knowledge");
        let sentinel = outside.path().join("sentinel.md");
        std::fs::write(&sentinel, "keep").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &mount_root).unwrap();
        #[cfg(windows)]
        junction::create(outside.path(), &mount_root).unwrap();

        let present = sync_mounts(ws.path(), Vec::new()).await;

        assert!(present.is_empty());
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep");
        assert!(read_link_target(&mount_root).is_some());
    }

    #[tokio::test]
    async fn existing_legacy_copy_is_replaced_by_live_link() {
        let bases = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let kb = make_base(&bases, "kb_copy");
        let mount_root = ws.path().join(KB_MOUNT_REL_DIR);
        let fallback = mount_root.join("copy");

        // Deterministically model an old release's copy fallback: the mounted
        // entry is a real directory rather than a symlink/junction.
        std::fs::create_dir_all(&fallback).unwrap();
        std::fs::write(fallback.join("note.md"), "old snapshot").unwrap();
        std::fs::write(fallback.join("stale.md"), "must disappear").unwrap();
        std::fs::write(kb.join("note.md"), "source v2").unwrap();
        std::fs::write(kb.join("fresh.md"), "fresh").unwrap();

        let spec = || {
            vec![MountSpec {
                link_name: "copy".into(),
                target: kb.clone(),
            }]
        };
        assert_eq!(sync_mounts(ws.path(), spec()).await, vec!["copy"]);
        assert!(
            read_link_target(&fallback).is_some(),
            "legacy copy must be replaced by a live link"
        );
        assert_eq!(std::fs::read_to_string(fallback.join("note.md")).unwrap(), "source v2");
        assert_eq!(std::fs::read_to_string(fallback.join("fresh.md")).unwrap(), "fresh");
        assert!(!fallback.join("stale.md").exists());

        // A second sync observes the live source directly; no refresh copy can
        // silently diverge from write-back.
        std::fs::write(kb.join("note.md"), "source v3").unwrap();
        std::fs::remove_file(kb.join("fresh.md")).unwrap();
        std::fs::write(kb.join("later.md"), "later").unwrap();
        assert_eq!(sync_mounts(ws.path(), spec()).await, vec!["copy"]);
        assert_eq!(std::fs::read_to_string(fallback.join("note.md")).unwrap(), "source v3");
        assert!(!fallback.join("fresh.md").exists());
        assert_eq!(std::fs::read_to_string(fallback.join("later.md")).unwrap(), "later");
    }

    #[tokio::test]
    async fn hostile_nomi_or_knowledge_alias_is_never_followed() {
        let bases = TempDir::new().unwrap();
        let kb = make_base(&bases, "kb_hostile");

        // Case 1: `.nomi` itself redirects outside the workspace.
        let ws_nomi = TempDir::new().unwrap();
        let outside_nomi = TempDir::new().unwrap();
        std::fs::create_dir_all(outside_nomi.path().join("knowledge")).unwrap();
        let victim_nomi = outside_nomi.path().join("knowledge").join("victim.md");
        std::fs::write(&victim_nomi, "outside").unwrap();
        let nomi_alias = ws_nomi.path().join(".nomi");
        create_directory_alias(outside_nomi.path(), &nomi_alias);

        assert!(
            sync_mounts(
                ws_nomi.path(),
                vec![MountSpec {
                    link_name: "safe".into(),
                    target: kb.clone(),
                }],
            )
            .await
            .is_empty()
        );
        assert!(sync_mounts(ws_nomi.path(), vec![]).await.is_empty());
        assert_eq!(std::fs::read_to_string(&victim_nomi).unwrap(), "outside");
        assert!(!outside_nomi.path().join("knowledge").join("safe").exists());
        remove_directory_alias(&nomi_alias);

        // Case 2: `.nomi` is real but `.nomi/knowledge` redirects outside.
        let ws_knowledge = TempDir::new().unwrap();
        let outside_knowledge = TempDir::new().unwrap();
        std::fs::create_dir(ws_knowledge.path().join(".nomi")).unwrap();
        let victim_knowledge = outside_knowledge.path().join("victim.md");
        std::fs::write(&victim_knowledge, "outside").unwrap();
        let knowledge_alias = ws_knowledge.path().join(KB_MOUNT_REL_DIR);
        create_directory_alias(outside_knowledge.path(), &knowledge_alias);

        assert!(
            sync_mounts(
                ws_knowledge.path(),
                vec![MountSpec {
                    link_name: "safe".into(),
                    target: kb,
                }],
            )
            .await
            .is_empty()
        );
        assert!(sync_mounts(ws_knowledge.path(), vec![]).await.is_empty());
        assert_eq!(std::fs::read_to_string(&victim_knowledge).unwrap(), "outside");
        assert!(!outside_knowledge.path().join("safe").exists());
        remove_directory_alias(&knowledge_alias);
    }

    #[tokio::test]
    async fn stale_link_entry_is_unlinked_without_deleting_its_target() {
        let bases = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let kb = make_base(&bases, "kb_safe");
        let outside = TempDir::new().unwrap();
        let victim = outside.path().join("victim.md");
        std::fs::write(&victim, "outside").unwrap();

        let mount_root = ws.path().join(KB_MOUNT_REL_DIR);
        std::fs::create_dir_all(&mount_root).unwrap();
        let hostile_entry = mount_root.join("hostile");
        create_directory_alias(outside.path(), &hostile_entry);

        assert_eq!(
            sync_mounts(
                ws.path(),
                vec![MountSpec {
                    link_name: "safe".into(),
                    target: kb,
                }],
            )
            .await,
            vec!["safe"]
        );
        assert!(matches!(classify_path(&hostile_entry), Ok(PathKind::Missing)));
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "outside");
    }

    #[tokio::test]
    async fn nested_alias_inside_legacy_copy_is_not_traversed_during_replacement() {
        let bases = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let kb = make_base(&bases, "kb_nested_alias");
        let outside = TempDir::new().unwrap();
        let victim = outside.path().join("victim.md");
        std::fs::write(&victim, "outside").unwrap();
        let fallback = ws.path().join(KB_MOUNT_REL_DIR).join("copy");
        std::fs::create_dir_all(&fallback).unwrap();
        create_directory_alias(outside.path(), &fallback.join("nested"));

        assert_eq!(
            sync_mounts(
                ws.path(),
                vec![MountSpec {
                    link_name: "copy".into(),
                    target: kb,
                }],
            )
            .await,
            vec!["copy"]
        );
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "outside");
        assert!(read_link_target(&fallback).is_some());
        assert!(!fallback.join("nested").exists());
    }

    #[tokio::test]
    async fn unsafe_link_name_cannot_escape_mount_root() {
        let bases = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let kb = make_base(&bases, "kb_escape");
        let escaped = ws.path().join(".nomi").join("escaped");

        for unsafe_name in ["../escaped", "CON", "nul.txt", ".NOMI-COPY-hostile"] {
            assert!(
                sync_mounts(
                    ws.path(),
                    vec![MountSpec {
                        link_name: unsafe_name.into(),
                        target: kb.clone(),
                    }],
                )
                .await
                .is_empty()
            );
        }
        assert!(!escaped.exists());
    }

    #[tokio::test]
    async fn canonical_workspace_aliases_share_one_sync_mutex() {
        let ws = TempDir::new().unwrap();
        let direct = workspace_sync_lock(ws.path());
        let lexical_alias = workspace_sync_lock(&ws.path().join("."));
        assert!(
            Arc::ptr_eq(&direct, &lexical_alias),
            "the same workspace spelling aliases must serialize one two-pass reconcile"
        );

        let guard = direct.lock().await;
        assert!(
            lexical_alias.try_lock().is_err(),
            "a concurrent reconcile must wait for the workspace owner"
        );
        drop(guard);
        assert!(lexical_alias.try_lock().is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_async_caller_keeps_lock_until_blocking_sync_exits() {
        const HELPER_MODE: &str = "NOMIFUN_CANCELLED_MOUNT_AUTHORITY_HELPER_MODE";
        const HELPER_WORKSPACE: &str =
            "NOMIFUN_CANCELLED_MOUNT_AUTHORITY_HELPER_WORKSPACE";
        const TEST_NAME: &str =
            "mount::tests::cancelling_async_caller_keeps_lock_until_blocking_sync_exits";

        if let Some(mode) = std::env::var_os(HELPER_MODE) {
            let workspace = PathBuf::from(
                std::env::var_os(HELPER_WORKSPACE)
                    .expect("cancelled mount helper workspace"),
            );
            let result = WorkspaceBindingLease::acquire(
                &workspace,
                "binding-after-cancel",
                "second-process",
            );
            match mode.to_string_lossy().as_ref() {
                "must-conflict" => assert!(
                    matches!(result, Err(AppError::Conflict(_))),
                    "a second process must not acquire while cancelled activation still reconciles"
                ),
                "must-acquire" => {
                    result.expect("authority must release after blocking reconcile exits");
                }
                other => panic!("unknown cancelled mount helper mode {other}"),
            }
            return;
        }

        let ws = TempDir::new().unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let pause = Arc::new(BlockingSyncTestPause {
            workspace_key: workspace_lock_key(ws.path()),
            entered: StdMutex::new(Some(entered_tx)),
            released: StdMutex::new(false),
            release_signal: std::sync::Condvar::new(),
        });
        *blocking_sync_test_pause_slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&pause));

        let authority = WorkspaceBindingLease::acquire(
            ws.path(),
            "binding-during-activation",
            "cancelled-activation",
        )
        .unwrap();
        let workspace = ws.path().to_path_buf();
        let task = tokio::spawn(async move {
            sync_mounts_with_authority(&workspace, Vec::new(), authority).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
            .await
            .expect("blocking reconcile must start")
            .expect("blocking reconcile must signal");
        *blocking_sync_test_pause_slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let same_lock = workspace_sync_lock(ws.path());
        let remained_locked = same_lock.try_lock().is_err();
        let run_helper = |mode: &str| {
            std::process::Command::new(
                std::env::current_exe().expect("current test executable"),
            )
            .arg("--exact")
            .arg(TEST_NAME)
            .arg("--nocapture")
            .env(HELPER_MODE, mode)
            .env(HELPER_WORKSPACE, ws.path())
            .output()
            .expect("spawn cancelled mount authority helper")
        };
        let blocked = run_helper("must-conflict");
        assert!(
            blocked.status.success(),
            "second-process conflict assertion failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&blocked.stdout),
            String::from_utf8_lossy(&blocked.stderr)
        );

        *pause
            .released
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        pause.release_signal.notify_all();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if same_lock.try_lock().is_ok() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("blocking reconcile must eventually release its owned guard");
        assert!(
            remained_locked,
            "aborting the async waiter must not unlock a running filesystem reconcile"
        );

        let successor = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match WorkspaceBindingLease::acquire(
                    ws.path(),
                    "binding-after-cancel",
                    "same-process-successor",
                ) {
                    Ok(lease) => break lease,
                    Err(AppError::Conflict(_)) => tokio::task::yield_now().await,
                    Err(error) => panic!("unexpected successor authority error: {error}"),
                }
            }
        })
        .await
        .expect("blocking reconcile must release cross-process authority");
        drop(successor);

        let acquired = run_helper("must-acquire");
        assert!(
            acquired.status.success(),
            "second process did not acquire after reconcile exit\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&acquired.stdout),
            String::from_utf8_lossy(&acquired.stderr)
        );
    }

    #[tokio::test]
    async fn case_colliding_mount_names_are_rejected_consistently() {
        let bases = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let kb_first = make_base(&bases, "kb_case_first");
        let kb_second = make_base(&bases, "kb_case_second");
        std::fs::write(kb_first.join("identity.md"), "first").unwrap();
        std::fs::write(kb_second.join("identity.md"), "second").unwrap();

        let present = sync_mounts(
            ws.path(),
            vec![
                MountSpec {
                    link_name: "Docs".into(),
                    target: kb_first,
                },
                MountSpec {
                    link_name: "docs".into(),
                    target: kb_second,
                },
            ],
        )
        .await;
        assert_eq!(present, vec!["Docs"]);
        assert_eq!(
            std::fs::read_to_string(
                ws.path()
                    .join(KB_MOUNT_REL_DIR)
                    .join("Docs")
                    .join("identity.md")
            )
            .unwrap(),
            "first"
        );
    }

    #[tokio::test]
    async fn corrupt_mixed_case_gitignore_is_rebuilt_canonically() {
        let bases = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let kb = make_base(&bases, "kb_ignore");
        let mount_root = ws.path().join(KB_MOUNT_REL_DIR);
        std::fs::create_dir_all(&mount_root).unwrap();
        std::fs::write(mount_root.join(".GITIGNORE"), "not ignored\n").unwrap();

        assert_eq!(
            sync_mounts(
                ws.path(),
                vec![MountSpec {
                    link_name: "kb".into(),
                    target: kb,
                }],
            )
            .await,
            vec!["kb"]
        );
        assert_eq!(std::fs::read(mount_root.join(".gitignore")).unwrap(), b"*\n");
        let aliases = std::fs::read_dir(&mount_root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(".gitignore")
            })
            .collect::<Vec<_>>();
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].file_name(), OsStr::new(".gitignore"));
    }

    #[tokio::test]
    async fn hostile_managed_readme_alias_or_hardlink_is_removed_without_touching_target() {
        let bases = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let kb = make_base(&bases, "kb_readme");
        let outside = TempDir::new().unwrap();
        let victim = outside.path().join("victim.md");
        std::fs::write(&victim, "outside").unwrap();
        let mount_root = ws.path().join(KB_MOUNT_REL_DIR);
        std::fs::create_dir_all(&mount_root).unwrap();
        let readme = mount_root.join("README.md");
        std::fs::hard_link(&victim, &readme).unwrap();

        let spec = || {
            vec![MountSpec {
                link_name: "kb".into(),
                target: kb.clone(),
            }]
        };
        assert_eq!(sync_mounts(ws.path(), spec()).await, vec!["kb"]);
        assert!(matches!(classify_path(&readme), Ok(PathKind::Missing)));
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "outside");

        create_directory_alias(outside.path(), &readme);
        assert_eq!(sync_mounts(ws.path(), spec()).await, vec!["kb"]);
        assert!(matches!(classify_path(&readme), Ok(PathKind::Missing)));
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "outside");
    }

    #[tokio::test]
    async fn terminal_readme_publish_atomically_replaces_regular_hardlink_and_alias() {
        let bases = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let kb = make_base(&bases, "kb_terminal_readme");
        assert_eq!(
            sync_mounts(
                ws.path(),
                vec![MountSpec {
                    link_name: "kb".into(),
                    target: kb,
                }],
            )
            .await,
            vec!["kb"]
        );
        let readme = ws.path().join(KB_MOUNT_REL_DIR).join("README.md");
        write_terminal_readme(ws.path(), "first").await.unwrap();
        assert_eq!(std::fs::read_to_string(&readme).unwrap(), "first");

        let outside = TempDir::new().unwrap();
        let victim = outside.path().join("victim.md");
        std::fs::write(&victim, "outside").unwrap();
        std::fs::remove_file(&readme).unwrap();
        std::fs::hard_link(&victim, &readme).unwrap();
        write_terminal_readme(ws.path(), "second").await.unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "outside");
        assert_eq!(std::fs::read_to_string(&readme).unwrap(), "second");
        assert!(!metadata_has_multiple_links(&readme));

        std::fs::remove_file(&readme).unwrap();
        create_directory_alias(outside.path(), &readme);
        write_terminal_readme(ws.path(), "third").await.unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "outside");
        assert_eq!(std::fs::read_to_string(&readme).unwrap(), "third");
        assert!(matches!(classify_path(&readme), Ok(PathKind::File)));
    }

    #[tokio::test]
    async fn terminal_readme_publish_rejects_hostile_mount_root_alias() {
        let ws = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let victim = outside.path().join("victim.md");
        std::fs::write(&victim, "outside").unwrap();
        std::fs::create_dir(ws.path().join(".nomi")).unwrap();
        let mount_alias = ws.path().join(KB_MOUNT_REL_DIR);
        create_directory_alias(outside.path(), &mount_alias);

        assert!(write_terminal_readme(ws.path(), "hostile").await.is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "outside");
        assert!(!outside.path().join("README.md").exists());
        remove_directory_alias(&mount_alias);
    }

    #[tokio::test]
    async fn relative_target_is_canonicalized_before_link_creation() {
        let current_dir = std::env::current_dir().unwrap();
        let bases = tempfile::Builder::new()
            .prefix("nomifun-relative-kb-")
            .tempdir_in(&current_dir)
            .unwrap();
        let ws = TempDir::new().unwrap();
        let kb = make_base(&bases, "kb_relative");
        let relative = kb.strip_prefix(&current_dir).unwrap().to_path_buf();

        assert_eq!(
            sync_mounts(
                ws.path(),
                vec![MountSpec {
                    link_name: "relative".into(),
                    target: relative,
                }],
            )
            .await,
            vec!["relative"]
        );
        assert!(
            ws.path()
                .join(KB_MOUNT_REL_DIR)
                .join("relative")
                .join("note.md")
                .is_file()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reconciles_leave_one_complete_desired_set() {
        let bases = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let kb_a1 = make_base(&bases, "kb_a1");
        let kb_a2 = make_base(&bases, "kb_a2");
        let kb_b1 = make_base(&bases, "kb_b1");
        let kb_b2 = make_base(&bases, "kb_b2");

        for _ in 0..8 {
            let barrier = Arc::new(tokio::sync::Barrier::new(3));
            let workspace_a = ws.path().to_path_buf();
            let workspace_b = ws.path().join(".");
            let barrier_a = Arc::clone(&barrier);
            let barrier_b = Arc::clone(&barrier);
            let a1 = kb_a1.clone();
            let a2 = kb_a2.clone();
            let b1 = kb_b1.clone();
            let b2 = kb_b2.clone();
            let reconcile_a = tokio::spawn(async move {
                barrier_a.wait().await;
                sync_mounts(
                    &workspace_a,
                    vec![
                        MountSpec {
                            link_name: "a1".into(),
                            target: a1,
                        },
                        MountSpec {
                            link_name: "a2".into(),
                            target: a2,
                        },
                    ],
                )
                .await
            });
            let reconcile_b = tokio::spawn(async move {
                barrier_b.wait().await;
                sync_mounts(
                    &workspace_b,
                    vec![
                        MountSpec {
                            link_name: "b1".into(),
                            target: b1,
                        },
                        MountSpec {
                            link_name: "b2".into(),
                            target: b2,
                        },
                    ],
                )
                .await
            });
            barrier.wait().await;
            assert_eq!(reconcile_a.await.unwrap().len(), 2);
            assert_eq!(reconcile_b.await.unwrap().len(), 2);

            let actual = std::fs::read_dir(ws.path().join(KB_MOUNT_REL_DIR))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| !MANAGED_KEEP.contains(&name.as_str()))
                .collect::<HashSet<_>>();
            let desired_a = HashSet::from(["a1".to_string(), "a2".to_string()]);
            let desired_b = HashSet::from(["b1".to_string(), "b2".to_string()]);
            assert!(
                actual == desired_a || actual == desired_b,
                "serialized two-pass sync must not leave a mixed mount set: {actual:?}"
            );
        }
    }

    #[tokio::test]
    async fn writes_through_mount_reach_target() {
        let bases = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let kb = make_base(&bases, "kb_w");

        sync_mounts(
            ws.path(),
            vec![MountSpec {
                link_name: "w".into(),
                target: kb.clone(),
            }],
        )
        .await;

        let mounted = ws.path().join(KB_MOUNT_REL_DIR).join("w");
        assert!(
            read_link_target(&mounted).is_some(),
            "a reported mount must be a live link, never a detached copy"
        );
        std::fs::write(mounted.join("written.md"), "wb").unwrap();
        assert!(kb.join("written.md").exists(), "write-back must land in the base root");
    }
}

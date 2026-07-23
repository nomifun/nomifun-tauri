//! Verified, workspace-scoped persistence for binary outputs produced by agents.
//!
//! A provider/tool reporting success is not proof that an artifact exists.  This
//! module is the delivery boundary: bytes are decoded, bounded, format-checked,
//! written atomically, and read back before a caller may publish `Completed`.

use std::fs::{self, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use nomifun_common::PersistedArtifactId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const ARTIFACT_DIRECTORY: &str = "nomifun-artifacts";
const MAX_INLINE_ARTIFACT_BYTES: usize = 20 * 1024 * 1024;
const MAX_EXISTING_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_IMAGE_DECODE_ALLOC: u64 = 256 * 1024 * 1024;
const MAX_ZIP_ENTRIES: usize = 10_000;
const MAX_ZIP_EXPANDED_BYTES: u64 = MAX_EXISTING_ARTIFACT_BYTES;
const MAX_OGG_PACKET_BYTES: usize = 16 * 1024 * 1024;
const STALE_TEMP_AGE: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Image,
    Audio,
    Video,
    Text,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedArtifact {
    pub id: String,
    pub kind: ArtifactKind,
    pub mime_type: String,
    /// Canonical native path. This is directly readable on the current host.
    pub path: String,
    /// Portable workspace-relative locator, always using `/` separators.
    pub relative_path: String,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactStoreError {
    #[error("artifact payload is empty")]
    Empty,
    #[error("artifact payload exceeds the {limit} byte limit")]
    TooLarge { limit: usize },
    #[error("artifact payload is not valid base64")]
    InvalidBase64,
    #[error("unsupported or invalid artifact MIME type: {0}")]
    InvalidMime(String),
    #[error("artifact bytes do not match declared MIME type {declared} (detected {detected})")]
    MimeMismatch { declared: String, detected: String },
    #[error("image payload cannot be decoded safely: {0}")]
    InvalidImage(String),
    #[error("artifact directory escapes the workspace boundary")]
    OutsideWorkspace,
    #[error("artifact storage failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("artifact verification failed after writing")]
    VerificationFailed,
}

#[derive(Debug, Clone)]
pub struct ArtifactStore {
    workspace_root: PathBuf,
    artifact_root: PathBuf,
}

#[derive(Debug)]
struct ValidatedArtifact {
    kind: ArtifactKind,
    mime_type: String,
    extension: &'static str,
    bytes: Vec<u8>,
    sha256: String,
}

impl ArtifactStore {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        let workspace_root = workspace_root.into();
        let artifact_root = workspace_root.join(ARTIFACT_DIRECTORY);
        Self {
            workspace_root,
            artifact_root,
        }
    }

    pub fn artifact_root(&self) -> &Path {
        &self.artifact_root
    }

    /// Persist an all-or-nothing batch of inline images.
    pub fn persist_images<I, M, D>(&self, images: I) -> Result<Vec<PersistedArtifact>, ArtifactStoreError>
    where
        I: IntoIterator<Item = (M, D)>,
        M: AsRef<str>,
        D: AsRef<str>,
    {
        let validated = images
            .into_iter()
            .map(|(mime, data)| validate_inline(ArtifactKind::Image, mime.as_ref(), data.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        if validated.is_empty() {
            return Err(ArtifactStoreError::Empty);
        }
        self.persist_validated_batch(validated)
    }

    /// Persist a heterogeneous all-or-nothing batch of inline artifacts.
    ///
    /// Every payload is decoded and kind/MIME validated before the first file
    /// is created. This is the MCP delivery entry point for image, audio,
    /// video, text and generic file results.
    pub fn persist_inline_batch<I, M, D>(
        &self,
        artifacts: I,
    ) -> Result<Vec<PersistedArtifact>, ArtifactStoreError>
    where
        I: IntoIterator<Item = (ArtifactKind, M, D)>,
        M: AsRef<str>,
        D: AsRef<str>,
    {
        let validated = artifacts
            .into_iter()
            .map(|(kind, mime, data)| validate_inline(kind, mime.as_ref(), data.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        if validated.is_empty() {
            return Err(ArtifactStoreError::Empty);
        }
        self.persist_validated_batch(validated)
    }

    /// Persist one ACP/MCP inline artifact after kind-specific validation.
    pub fn persist_inline(
        &self,
        kind: ArtifactKind,
        mime_type: &str,
        base64_data: &str,
    ) -> Result<PersistedArtifact, ArtifactStoreError> {
        let validated = validate_inline(kind, mime_type, base64_data)?;
        self.persist_validated_batch(vec![validated])?
            .into_iter()
            .next()
            .ok_or(ArtifactStoreError::VerificationFailed)
    }

    /// Persist a textual resource as a real file instead of leaving it only in
    /// a transient protocol frame.
    pub fn persist_text(
        &self,
        mime_type: Option<&str>,
        text: &str,
    ) -> Result<PersistedArtifact, ArtifactStoreError> {
        if text.len() > MAX_INLINE_ARTIFACT_BYTES {
            return Err(ArtifactStoreError::TooLarge {
                limit: MAX_INLINE_ARTIFACT_BYTES,
            });
        }
        let mime = normalize_mime(mime_type.unwrap_or("text/plain"));
        let bytes = text.as_bytes().to_vec();
        let extension = validate_text_file(&mime, &bytes)?;
        let sha256 = hex::encode(Sha256::digest(&bytes));
        self.persist_validated_batch(vec![ValidatedArtifact {
            kind: ArtifactKind::Text,
            mime_type: mime,
            extension,
            bytes,
            sha256,
        }])?
        .into_iter()
        .next()
        .ok_or(ArtifactStoreError::VerificationFailed)
    }

    /// Build a verified receipt for a tool-created file that already lives in
    /// the conversation workspace. The file is not copied: its canonical
    /// workspace-bound path, regular-file status, non-empty size, format (for
    /// known media/document types), and SHA-256 are checked before the receipt
    /// can be published as a successful artifact.
    pub fn verify_existing_path(&self, path: impl AsRef<Path>) -> Result<PersistedArtifact, ArtifactStoreError> {
        let workspace = fs::canonicalize(&self.workspace_root)?;
        let requested = path.as_ref();
        let candidate = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            workspace.join(requested)
        };
        let canonical_path = fs::canonicalize(candidate)?;
        if !canonical_path.starts_with(&workspace) {
            return Err(ArtifactStoreError::OutsideWorkspace);
        }

        let metadata = fs::metadata(&canonical_path)?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(ArtifactStoreError::VerificationFailed);
        }
        if metadata.len() > MAX_EXISTING_ARTIFACT_BYTES {
            return Err(ArtifactStoreError::TooLarge {
                limit: MAX_EXISTING_ARTIFACT_BYTES as usize,
            });
        }

        // Known media and document formats must be checked over the complete
        // container. Reading only a prefix lets a header-only or truncated file
        // acquire a success receipt. The metadata limit above bounds this read.
        let bytes = fs::read(&canonical_path)?;
        let metadata_after_read = fs::metadata(&canonical_path)?;
        if !metadata_after_read.is_file()
            || metadata_after_read.len() != metadata.len()
            || bytes.len() as u64 != metadata.len()
        {
            return Err(ArtifactStoreError::VerificationFailed);
        }
        let (kind, mime_type, _) = validate_existing_file(&canonical_path, &bytes)?;
        let relative = canonical_path
            .strip_prefix(&workspace)
            .map_err(|_| ArtifactStoreError::OutsideWorkspace)?;
        let canonical_locator = canonical_path
            .to_str()
            .ok_or(ArtifactStoreError::VerificationFailed)?;
        let relative_locator = relative
            .to_str()
            .ok_or(ArtifactStoreError::VerificationFailed)?;

        Ok(PersistedArtifact {
            id: PersistedArtifactId::new().into_string(),
            kind,
            mime_type,
            path: canonical_locator.to_owned(),
            relative_path: relative_locator.replace('\\', "/"),
            size_bytes: metadata.len(),
            sha256: hex::encode(Sha256::digest(&bytes)),
        })
    }

    /// Re-verify a previously published receipt against its current durable
    /// bytes. Turn-finalization calls this after all tools have stopped, so a
    /// later shell/edit call cannot delete or replace an earlier snapshot and
    /// still leave the accepted turn in a successful state.
    pub fn reverify_receipt(
        &self,
        receipt: &PersistedArtifact,
    ) -> Result<(), ArtifactStoreError> {
        let verified = self.verify_existing_path(&receipt.path)?;
        if verified.kind != receipt.kind
            || verified.mime_type != receipt.mime_type
            || verified.path != receipt.path
            || verified.relative_path != receipt.relative_path
            || verified.size_bytes != receipt.size_bytes
            || !verified.sha256.eq_ignore_ascii_case(&receipt.sha256)
        {
            return Err(ArtifactStoreError::VerificationFailed);
        }
        Ok(())
    }

    /// Import a tool-created workspace file into the immutable artifact area.
    ///
    /// Unlike [`Self::verify_existing_path`], the returned receipt never points
    /// at the caller-owned source path. The source is fully validated, then read
    /// a second time and compared by size and SHA-256 immediately before the
    /// already-validated bytes are atomically committed. A later overwrite of
    /// the source therefore cannot mutate a published artifact receipt.
    pub fn import_existing_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<PersistedArtifact, ArtifactStoreError> {
        self.import_existing_batch(std::iter::once(path))?
            .into_iter()
            .next()
            .ok_or(ArtifactStoreError::VerificationFailed)
    }

    /// Atomically import multiple existing workspace files. Every source is
    /// validated and stability-checked before the first snapshot is written;
    /// `persist_validated_batch` then provides rollback if a commit fails.
    pub fn import_existing_batch<I, P>(&self, paths: I) -> Result<Vec<PersistedArtifact>, ArtifactStoreError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let prepared = self.prepare_existing_batch(paths)?;
        if prepared.is_empty() {
            return Err(ArtifactStoreError::Empty);
        }
        self.persist_validated_batch(prepared)
    }

    /// Persist inline payloads and tool-created workspace files as one atomic
    /// delivery. This is used when a single terminal tool result mixes both
    /// transport forms: validation of every member and source-stability checks
    /// finish before the first immutable receipt is committed.
    pub fn persist_inline_and_existing_batch<I, M, D, E, P>(
        &self,
        inline: I,
        existing_paths: E,
    ) -> Result<Vec<PersistedArtifact>, ArtifactStoreError>
    where
        I: IntoIterator<Item = (ArtifactKind, M, D)>,
        M: AsRef<str>,
        D: AsRef<str>,
        E: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut existing = self.prepare_existing_batch(existing_paths)?;
        let mut validated_inline = inline
            .into_iter()
            .map(|(kind, mime, data)| validate_inline(kind, mime.as_ref(), data.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        existing.append(&mut validated_inline);
        if existing.is_empty() {
            return Err(ArtifactStoreError::Empty);
        }
        self.persist_validated_batch(existing)
    }

    fn prepare_existing_batch<I, P>(
        &self,
        paths: I,
    ) -> Result<Vec<ValidatedArtifact>, ArtifactStoreError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let workspace = fs::canonicalize(&self.workspace_root)?;
        let mut prepared = Vec::<(PathBuf, ValidatedArtifact)>::new();
        let mut sources = std::collections::HashSet::new();
        for requested in paths {
            let requested = requested.as_ref();
            let candidate = if requested.is_absolute() {
                requested.to_path_buf()
            } else {
                workspace.join(requested)
            };
            let canonical_path = fs::canonicalize(candidate)?;
            if !canonical_path.starts_with(&workspace) {
                return Err(ArtifactStoreError::OutsideWorkspace);
            }
            if !sources.insert(canonical_path.clone()) {
                return Err(ArtifactStoreError::VerificationFailed);
            }
            let metadata = fs::metadata(&canonical_path)?;
            if !metadata.is_file() || metadata.len() == 0 {
                return Err(ArtifactStoreError::VerificationFailed);
            }
            if metadata.len() > MAX_EXISTING_ARTIFACT_BYTES {
                return Err(ArtifactStoreError::TooLarge {
                    limit: MAX_EXISTING_ARTIFACT_BYTES as usize,
                });
            }
            let bytes = fs::read(&canonical_path)?;
            if bytes.len() as u64 != metadata.len() {
                return Err(ArtifactStoreError::VerificationFailed);
            }
            let (kind, mime_type, extension) = validate_existing_file(&canonical_path, &bytes)?;
            let sha256 = hex::encode(Sha256::digest(&bytes));
            prepared.push((
                canonical_path,
                ValidatedArtifact {
                    kind,
                    mime_type,
                    extension,
                    bytes,
                    sha256,
                },
            ));
        }
        // Re-read every source only after the whole batch has passed format
        // validation. A change to an earlier source while a later source was
        // being inspected therefore aborts before `nomifun-artifacts` exists.
        for (canonical_path, artifact) in &prepared {
            let current = fs::read(canonical_path)?;
            let current_metadata = fs::metadata(canonical_path)?;
            if !current_metadata.is_file()
                || current.len() != artifact.bytes.len()
                || current_metadata.len() != artifact.bytes.len() as u64
                || hex::encode(Sha256::digest(&current)) != artifact.sha256
            {
                return Err(ArtifactStoreError::VerificationFailed);
            }
        }
        Ok(prepared.into_iter().map(|(_, artifact)| artifact).collect())
    }

    fn prepare_root(&self) -> Result<PathBuf, ArtifactStoreError> {
        let workspace = fs::canonicalize(&self.workspace_root)?;
        fs::create_dir_all(&self.artifact_root)?;
        let artifact_root = fs::canonicalize(&self.artifact_root)?;
        if !artifact_root.starts_with(&workspace) {
            return Err(ArtifactStoreError::OutsideWorkspace);
        }
        cleanup_stale_temp_files(&artifact_root)?;
        Ok(artifact_root)
    }

    fn persist_validated_batch(
        &self,
        validated: Vec<ValidatedArtifact>,
    ) -> Result<Vec<PersistedArtifact>, ArtifactStoreError> {
        let artifact_root = self.prepare_root()?;
        let mut written: Vec<PathBuf> = Vec::with_capacity(validated.len());
        let mut output = Vec::with_capacity(validated.len());

        for item in validated {
            let id = PersistedArtifactId::new().into_string();
            // Keep the readable storage namespace in the file name only. The
            // durable/wire business identity remains a bare UUIDv7.
            let file_name = format!("artifact-{id}.{}", item.extension);
            let final_path = artifact_root.join(&file_name);
            let temp_path = artifact_root.join(format!(".artifact-{id}.tmp"));

            let mut published_by_this_batch = false;
            let result = (|| -> Result<PersistedArtifact, ArtifactStoreError> {
                let mut file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&temp_path)?;
                file.write_all(&item.bytes)?;
                file.sync_all()?;
                drop(file);

                durable_rename_no_replace(&temp_path, &final_path)?;
                published_by_this_batch = true;
                let read_back = fs::read(&final_path)?;
                if read_back.len() != item.bytes.len()
                    || hex::encode(Sha256::digest(&read_back)) != item.sha256
                {
                    return Err(ArtifactStoreError::VerificationFailed);
                }
                let canonical_path = fs::canonicalize(&final_path)?;
                if !canonical_path.starts_with(&artifact_root) {
                    return Err(ArtifactStoreError::OutsideWorkspace);
                }
                let canonical_locator = canonical_path
                    .to_str()
                    .ok_or(ArtifactStoreError::VerificationFailed)?;

                Ok(PersistedArtifact {
                    id,
                    kind: item.kind,
                    mime_type: item.mime_type,
                    path: canonical_locator.to_owned(),
                    relative_path: format!("{ARTIFACT_DIRECTORY}/{file_name}"),
                    size_bytes: read_back.len() as u64,
                    sha256: item.sha256,
                })
            })();

            match result {
                Ok(artifact) => {
                    written.push(final_path);
                    output.push(artifact);
                }
                Err(error) => {
                    cleanup_failed_publication(
                        &temp_path,
                        &final_path,
                        published_by_this_batch,
                    );
                    for path in written {
                        let _ = fs::remove_file(path);
                    }
                    let _ = sync_parent_directory(&artifact_root);
                    return Err(error);
                }
            }
        }

        Ok(output)
    }
}

fn cleanup_failed_publication(temp: &Path, target: &Path, target_is_owned: bool) {
    let _ = fs::remove_file(temp);
    // A no-replace collision means `target` belongs to somebody else. Only
    // compensate a destination after this exact batch successfully published
    // it.
    if target_is_owned {
        let _ = fs::remove_file(target);
    }
}

#[cfg(unix)]
fn sync_parent_directory(directory: &Path) -> std::io::Result<()> {
    // A synced file followed by rename is not crash-durable until the parent
    // directory entry is synced as well (Linux and macOS). Some writable
    // SMB/FUSE/removable-volume drivers reject directory fsync with EINVAL or
    // EOPNOTSUPP; the artifact file itself is already synced and is read back
    // byte-for-byte before a receipt is exposed, so treat only those explicit
    // "operation unsupported" forms as a supported-but-weaker durability
    // boundary. All real I/O/permission failures remain fail-closed.
    match fs::File::open(directory)?.sync_all() {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
fn sync_parent_directory(_directory: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn hard_link_no_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::hard_link(source, target)?;
    if let Err(error) = fs::remove_file(source) {
        let _ = fs::remove_file(target);
        return Err(error);
    }
    Ok(())
}

#[cfg(unix)]
fn reservation_rename_no_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    // Hard links are optional on several writable filesystems. Reserve the
    // fresh name without clobbering, then atomically rename the already-synced
    // private temp file over *our own unpublished reservation*. The artifact
    // store never exposes a locator until durable_rename_no_replace returns.
    let reservation = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)?;
    if let Err(error) = reservation.sync_all() {
        drop(reservation);
        let _ = fs::remove_file(target);
        return Err(error);
    }
    drop(reservation);
    if let Err(error) = fs::rename(source, target) {
        let _ = fs::remove_file(target);
        return Err(error);
    }
    Ok(())
}

#[cfg(unix)]
fn portable_commit_no_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    match hard_link_no_replace(source, target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(error),
        Err(_) => reservation_rename_no_replace(source, target),
    }
}

#[cfg(unix)]
fn commit_no_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source_c = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "artifact source path contains NUL")
    })?;
    let target_c = CString::new(target.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "artifact target path contains NUL")
    })?;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: both C strings are NUL-terminated and remain live for the
    // syscall. RENAME_NOREPLACE makes collision handling atomic.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source_c.as_ptr(),
            libc::AT_FDCWD,
            target_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    // SAFETY: both C strings are NUL-terminated and remain live for the call.
    // RENAME_EXCL is the Darwin no-replace equivalent.
    let result = unsafe { libc::renamex_np(source_c.as_ptr(), target_c.as_ptr(), libc::RENAME_EXCL) }
        as libc::c_long;

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    let result = {
        // Portable Unix fallback: hard-link creation is atomic and refuses an
        // existing destination. Both paths are always in the same directory.
        hard_link_no_replace(source, target)?;
        0
    };

    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let unsupported = matches!(
        error.raw_os_error(),
        Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)
    );
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let unsupported = matches!(error.raw_os_error(), Some(libc::EINVAL | libc::ENOTSUP));
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    let unsupported = false;

    if unsupported {
        // Old Linux kernels and some network filesystems do not implement the
        // platform no-replace rename. Prefer same-directory hard-link
        // publication, then fall back to a create_new reservation on filesystems
        // that do not implement hard links either.
        portable_commit_no_replace(source, target)
    } else {
        Err(error)
    }
}

#[cfg(unix)]
fn durable_rename_no_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    commit_no_replace(source, target)?;
    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "artifact target has no parent directory",
        )
    })?;
    if let Err(error) = sync_parent_directory(parent) {
        let _ = fs::remove_file(target);
        let _ = sync_parent_directory(parent);
        return Err(error);
    }
    Ok(())
}

#[cfg(windows)]
fn durable_rename_no_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: both path buffers are NUL-terminated UTF-16 and remain live for
    // the synchronous call. Deliberately omit MOVEFILE_REPLACE_EXISTING: an
    // unexpected collision fails closed rather than overwriting user data.
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn durable_rename_no_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    if target.try_exists()? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "artifact destination already exists",
        ));
    }
    fs::rename(source, target)
}

fn cleanup_stale_temp_files(directory: &Path) -> std::io::Result<()> {
    let Some(cutoff) = SystemTime::now().checked_sub(STALE_TEMP_AGE) else {
        return Ok(());
    };
    cleanup_temp_files_before(directory, cutoff)
}

fn cleanup_temp_files_before(directory: &Path, cutoff: SystemTime) -> std::io::Result<()> {
    let mut removed = false;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(".artifact-") || !name.ends_with(".tmp") || !entry.file_type()?.is_file() {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.modified().is_ok_and(|modified| modified <= cutoff) {
            fs::remove_file(entry.path())?;
            removed = true;
        }
    }
    if removed {
        sync_parent_directory(directory)?;
    }
    Ok(())
}

fn validate_existing_file(
    path: &Path,
    bytes: &[u8],
) -> Result<(ArtifactKind, String, &'static str), ArtifactStoreError> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if extension == "webm" {
        let info = webm_info(bytes).ok_or_else(|| format_mismatch("video/webm"))?;
        return if info.has_video {
            Ok((ArtifactKind::Video, "video/webm".to_owned(), "webm"))
        } else if info.has_audio {
            Ok((ArtifactKind::Audio, "audio/webm".to_owned(), "webm"))
        } else {
            Err(format_mismatch("video/webm"))
        };
    }
    let declared_mime = mime_for_extension(&extension).unwrap_or("application/octet-stream");

    if declared_mime.starts_with("text/")
        || matches!(declared_mime, "application/json" | "application/xml")
    {
        validate_file(ArtifactKind::Text, declared_mime, bytes)
    } else {
        validate_file(ArtifactKind::File, declared_mime, bytes)
    }
}

fn mime_for_extension(extension: &str) -> Option<&'static str> {
    match extension {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        "wav" => Some("audio/wav"),
        "ogg" => Some("audio/ogg"),
        "flac" => Some("audio/flac"),
        "mp3" => Some("audio/mpeg"),
        "m4a" => Some("audio/mp4"),
        "mp4" | "m4v" => Some("video/mp4"),
        "mov" => Some("video/quicktime"),
        "webm" => Some("video/webm"),
        "txt" => Some("text/plain"),
        "md" | "markdown" => Some("text/markdown"),
        "html" | "htm" => Some("text/html"),
        "csv" => Some("text/csv"),
        "json" => Some("application/json"),
        "xml" => Some("application/xml"),
        "pdf" => Some("application/pdf"),
        "zip" => Some("application/zip"),
        "docx" => Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        "xlsx" => Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        "pptx" => Some("application/vnd.openxmlformats-officedocument.presentationml.presentation"),
        _ => None,
    }
}

fn decode_base64_payload(declared_mime: &str, input: &str) -> Result<(String, Vec<u8>), ArtifactStoreError> {
    let declared_mime = normalize_mime(declared_mime);
    let (mime, encoded) = if let Some(rest) = input.strip_prefix("data:") {
        let (header, encoded) = rest.split_once(',').ok_or(ArtifactStoreError::InvalidBase64)?;
        let header = header
            .strip_suffix(";base64")
            .ok_or(ArtifactStoreError::InvalidBase64)?;
        let embedded_mime = normalize_mime(header);
        if !declared_mime.is_empty() && declared_mime != embedded_mime {
            return Err(ArtifactStoreError::MimeMismatch {
                declared: declared_mime,
                detected: embedded_mime,
            });
        }
        (embedded_mime, encoded)
    } else {
        (declared_mime, input)
    };

    if encoded.is_empty() {
        return Err(ArtifactStoreError::Empty);
    }
    // Reject huge strings before allocating decoded storage.
    if encoded.len() > (MAX_INLINE_ARTIFACT_BYTES * 4 / 3) + 8 {
        return Err(ArtifactStoreError::TooLarge {
            limit: MAX_INLINE_ARTIFACT_BYTES,
        });
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(encoded))
        .map_err(|_| ArtifactStoreError::InvalidBase64)?;
    if bytes.is_empty() {
        return Err(ArtifactStoreError::Empty);
    }
    if bytes.len() > MAX_INLINE_ARTIFACT_BYTES {
        return Err(ArtifactStoreError::TooLarge {
            limit: MAX_INLINE_ARTIFACT_BYTES,
        });
    }
    Ok((mime, bytes))
}

fn validate_inline(
    kind: ArtifactKind,
    declared_mime: &str,
    input: &str,
) -> Result<ValidatedArtifact, ArtifactStoreError> {
    let (declared_mime, bytes) = decode_base64_payload(declared_mime, input)?;
    if declared_mime.is_empty() {
        return Err(ArtifactStoreError::InvalidMime("empty MIME type".to_owned()));
    }
    let (kind, mime_type, extension) = match kind {
        ArtifactKind::Image => validate_image(&declared_mime, &bytes)?,
        ArtifactKind::Audio => validate_audio(&declared_mime, &bytes)?,
        ArtifactKind::Video => validate_video(&declared_mime, &bytes)?,
        ArtifactKind::Text | ArtifactKind::File => validate_file(kind, &declared_mime, &bytes)?,
    };
    let sha256 = hex::encode(Sha256::digest(&bytes));
    Ok(ValidatedArtifact {
        kind,
        mime_type,
        extension,
        bytes,
        sha256,
    })
}

fn validate_image(
    declared_mime: &str,
    bytes: &[u8],
) -> Result<(ArtifactKind, String, &'static str), ArtifactStoreError> {
    let format = image::guess_format(bytes).map_err(|error| ArtifactStoreError::InvalidImage(error.to_string()))?;
    let (detected_mime, extension) = match format {
        image::ImageFormat::Png => ("image/png", "png"),
        image::ImageFormat::Jpeg => ("image/jpeg", "jpg"),
        image::ImageFormat::WebP => ("image/webp", "webp"),
        image::ImageFormat::Gif => ("image/gif", "gif"),
        other => return Err(ArtifactStoreError::InvalidMime(format!("image/{other:?}"))),
    };
    let normalized_declared = match declared_mime {
        "image/jpg" => "image/jpeg",
        other => other,
    };
    if normalized_declared != detected_mime {
        return Err(ArtifactStoreError::MimeMismatch {
            declared: declared_mime.to_owned(),
            detected: detected_mime.to_owned(),
        });
    }

    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_DECODE_ALLOC);
    reader.limits(limits);
    reader
        .decode()
        .map_err(|error| ArtifactStoreError::InvalidImage(error.to_string()))?;
    Ok((ArtifactKind::Image, detected_mime.to_owned(), extension))
}

fn validate_audio(
    declared_mime: &str,
    bytes: &[u8],
) -> Result<(ArtifactKind, String, &'static str), ArtifactStoreError> {
    let (detected, extension) = if valid_wav(bytes) {
        ("audio/wav", "wav")
    } else if valid_ogg(bytes) {
        ("audio/ogg", "ogg")
    } else if valid_flac(bytes) {
        ("audio/flac", "flac")
    } else if valid_mp3(bytes) {
        ("audio/mpeg", "mp3")
    } else if webm_info(bytes).is_some_and(|info| info.has_audio) {
        ("audio/webm", "webm")
    } else if iso_bmff_info(bytes).is_some_and(|info| info.has_audio) {
        ("audio/mp4", "m4a")
    } else {
        return Err(ArtifactStoreError::InvalidMime(declared_mime.to_owned()));
    };
    if !mime_matches(declared_mime, detected) {
        return Err(ArtifactStoreError::MimeMismatch {
            declared: declared_mime.to_owned(),
            detected: detected.to_owned(),
        });
    }
    Ok((ArtifactKind::Audio, detected.to_owned(), extension))
}

fn validate_video(
    declared_mime: &str,
    bytes: &[u8],
) -> Result<(ArtifactKind, String, &'static str), ArtifactStoreError> {
    let (detected, extension) = if let Some(info) = iso_bmff_info(bytes).filter(|info| info.has_video) {
        if &info.brand == b"qt  " {
            ("video/quicktime", "mov")
        } else {
            ("video/mp4", "mp4")
        }
    } else if webm_info(bytes).is_some_and(|info| info.has_video) {
        ("video/webm", "webm")
    } else {
        return Err(ArtifactStoreError::InvalidMime(declared_mime.to_owned()));
    };
    if !mime_matches(declared_mime, detected) {
        return Err(ArtifactStoreError::MimeMismatch {
            declared: declared_mime.to_owned(),
            detected: detected.to_owned(),
        });
    }
    Ok((ArtifactKind::Video, detected.to_owned(), extension))
}

fn validate_file(
    kind: ArtifactKind,
    declared_mime: &str,
    bytes: &[u8],
) -> Result<(ArtifactKind, String, &'static str), ArtifactStoreError> {
    if bytes.is_empty() {
        return Err(ArtifactStoreError::Empty);
    }
    if declared_mime.starts_with("image/") {
        return validate_image(declared_mime, bytes);
    }
    if declared_mime.starts_with("audio/") {
        return validate_audio(declared_mime, bytes);
    }
    if declared_mime.starts_with("video/") {
        return validate_video(declared_mime, bytes);
    }
    if kind == ArtifactKind::Text
        || declared_mime.starts_with("text/")
        || matches!(declared_mime, "application/json" | "application/xml")
    {
        let extension = validate_text_file(declared_mime, bytes)?;
        return Ok((ArtifactKind::Text, declared_mime.to_owned(), extension));
    }

    if is_generic_binary_mime(declared_mime) {
        // Generic binary is allowed for genuinely unknown, non-empty data. If
        // the payload advertises a known format, validate and classify it so a
        // truncated PDF/ZIP/media file can never masquerade as an opaque .bin.
        if generic_binary_looks_like_error_document(bytes) {
            return Err(format_mismatch(declared_mime));
        }
        if let Some(sniffed) = sniff_known_mime(bytes) {
            return validate_file(ArtifactKind::File, sniffed, bytes);
        }
        return Ok((ArtifactKind::File, "application/octet-stream".to_owned(), "bin"));
    }

    match declared_mime {
        "application/pdf" if !valid_pdf(bytes) => {
            return Err(format_mismatch(declared_mime));
        }
        "application/zip" if !valid_zip(bytes, ZipFlavor::Generic) => {
            return Err(format_mismatch(declared_mime));
        }
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            if !valid_zip(bytes, ZipFlavor::Docx) =>
        {
            return Err(format_mismatch(declared_mime));
        }
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            if !valid_zip(bytes, ZipFlavor::Xlsx) =>
        {
            return Err(format_mismatch(declared_mime));
        }
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            if !valid_zip(bytes, ZipFlavor::Pptx) =>
        {
            return Err(format_mismatch(declared_mime));
        }
        _ => {}
    }
    let extension = extension_for_mime(declared_mime).unwrap_or("bin");
    Ok((kind, declared_mime.to_owned(), extension))
}

fn format_mismatch(declared_mime: &str) -> ArtifactStoreError {
    ArtifactStoreError::MimeMismatch {
        declared: declared_mime.to_owned(),
        detected: "unknown or corrupt".to_owned(),
    }
}

fn validate_text_file(mime: &str, bytes: &[u8]) -> Result<&'static str, ArtifactStoreError> {
    if !(mime.starts_with("text/") || matches!(mime, "application/json" | "application/xml")) {
        return Err(ArtifactStoreError::InvalidMime(mime.to_owned()));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ArtifactStoreError::InvalidMime(mime.to_owned()))?;
    if text.trim().is_empty() {
        return Err(ArtifactStoreError::Empty);
    }
    if mime == "application/json" && serde_json::from_str::<serde_json::Value>(text).is_err() {
        return Err(ArtifactStoreError::InvalidMime(mime.to_owned()));
    }
    Ok(extension_for_mime(mime).unwrap_or("txt"))
}

fn normalize_mime(value: &str) -> String {
    let mime = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match mime.as_str() {
        "image/jpg" | "image/pjpeg" => "image/jpeg".to_owned(),
        "audio/x-wav" => "audio/wav".to_owned(),
        "audio/mp3" => "audio/mpeg".to_owned(),
        "video/x-m4v" => "video/mp4".to_owned(),
        _ => mime,
    }
}

fn is_generic_binary_mime(mime: &str) -> bool {
    matches!(mime, "application/octet-stream" | "binary/octet-stream")
}

fn generic_binary_looks_like_error_document(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.len() > 256 * 1024 {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    let structured_error = (matches!(trimmed.as_bytes().first(), Some(b'{') | Some(b'['))
        && serde_json::from_str::<serde_json::Value>(trimmed).is_ok())
        || lower.starts_with("<!doctype html")
        || lower.starts_with("<html")
        || lower.starts_with("<head")
        || lower.starts_with("<body");
    structured_error
        || [
            "generation failed",
            "generation error",
            "artifact failed",
            "no artifact",
            "no output",
            "upstream failed",
            "request failed",
            "timed out",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

fn mime_matches(declared: &str, detected: &str) -> bool {
    declared == detected
        || (declared == "audio/x-wav" && detected == "audio/wav")
        || (declared == "audio/mp3" && detected == "audio/mpeg")
        || (declared == "video/x-m4v" && detected == "video/mp4")
}

fn read_be_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?))
}

fn read_le_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?))
}

const MAX_CONTAINER_ELEMENTS: usize = 10_000;

#[derive(Clone, Copy)]
struct BmffBox<'a> {
    kind: [u8; 4],
    payload: &'a [u8],
}

#[derive(Clone, Copy)]
struct BmffInfo {
    brand: [u8; 4],
    has_audio: bool,
    has_video: bool,
}

/// Parse one complete ISO-BMFF box sequence. Size-zero boxes are only valid as
/// the final box in their containing sequence; extended sizes are bounded by
/// `usize`, and an element cap prevents tiny-box allocation attacks.
fn bmff_boxes(bytes: &[u8]) -> Option<Vec<BmffBox<'_>>> {
    let mut offset = 0usize;
    let mut boxes = Vec::new();
    while offset < bytes.len() {
        if boxes.len() >= MAX_CONTAINER_ELEMENTS {
            return None;
        }
        let header = bytes.get(offset..offset.checked_add(8)?)?;
        let short_size = read_be_u32(header)? as usize;
        let kind: [u8; 4] = header[4..8].try_into().ok()?;
        let (header_len, box_len) = match short_size {
            0 => (8usize, bytes.len().checked_sub(offset)?),
            1 => {
                let wide = u64::from_be_bytes(bytes.get(offset + 8..offset + 16)?.try_into().ok()?);
                (16usize, usize::try_from(wide).ok()?)
            }
            size => (8usize, size),
        };
        let end = offset.checked_add(box_len)?;
        if box_len < header_len || end > bytes.len() || (short_size == 0 && end != bytes.len()) {
            return None;
        }
        boxes.push(BmffBox {
            kind,
            payload: bytes.get(offset + header_len..end)?,
        });
        offset = end;
    }
    (offset == bytes.len()).then_some(boxes)
}

fn bmff_box<'a>(boxes: &'a [BmffBox<'a>], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes.iter().find(|item| &item.kind == kind).map(|item| item.payload)
}

fn full_box_entry_count(payload: &[u8], entry_width: usize) -> Option<usize> {
    let count = usize::try_from(read_be_u32(payload.get(4..)?)?).ok()?;
    let required = 8usize.checked_add(count.checked_mul(entry_width)?)?;
    (count > 0 && payload.len() >= required).then_some(count)
}

fn valid_sample_description(payload: &[u8]) -> bool {
    let Some(count) = full_box_entry_count(payload, 8) else {
        return false;
    };
    let Some(entries) = bmff_boxes(&payload[8..]) else {
        return false;
    };
    entries.len() >= count
        && entries.iter().take(count).all(|entry| {
            entry.kind.iter().any(|byte| *byte != 0)
                && !entry.payload.is_empty()
                && entry.kind.iter().all(|byte| byte.is_ascii_graphic() || *byte == b' ')
        })
}

fn valid_sample_table(payload: &[u8], fragmented: bool) -> bool {
    let Some(boxes) = bmff_boxes(payload) else {
        return false;
    };
    if !bmff_box(&boxes, b"stsd").is_some_and(valid_sample_description) {
        return false;
    }
    if fragmented {
        return true;
    }

    let valid_stts = bmff_box(&boxes, b"stts").is_some_and(|value| {
        let Some(count) = full_box_entry_count(value, 8) else {
            return false;
        };
        (0..count).any(|index| {
            let start = 8 + index * 8;
            read_be_u32(&value[start..]).is_some_and(|samples| samples > 0)
        })
    });
    let valid_stsc = bmff_box(&boxes, b"stsc").is_some_and(|value| full_box_entry_count(value, 12).is_some());
    let valid_stsz = bmff_box(&boxes, b"stsz").is_some_and(|value| {
        let Some(sample_count) = read_be_u32(value.get(8..).unwrap_or_default())
            .and_then(|count| usize::try_from(count).ok())
        else {
            return false;
        };
        if sample_count == 0 || value.len() < 12 {
            return false;
        }
        let fixed_size = read_be_u32(value.get(4..).unwrap_or_default()).unwrap_or(0);
        fixed_size > 0 || value.len() >= 12usize.saturating_add(sample_count.saturating_mul(4))
    });
    let valid_chunk_offsets = bmff_box(&boxes, b"stco")
        .is_some_and(|value| full_box_entry_count(value, 4).is_some())
        || bmff_box(&boxes, b"co64").is_some_and(|value| full_box_entry_count(value, 8).is_some());
    valid_stts && valid_stsc && valid_stsz && valid_chunk_offsets
}

fn bmff_track_kind(payload: &[u8], fragmented: bool) -> Option<[u8; 4]> {
    let trak = bmff_boxes(payload)?;
    if !bmff_box(&trak, b"tkhd").is_some_and(|value| value.len() >= 8) {
        return None;
    }
    let mdia = bmff_boxes(bmff_box(&trak, b"mdia")?)?;
    if !bmff_box(&mdia, b"mdhd").is_some_and(|value| value.len() >= 8) {
        return None;
    }
    let handler = bmff_box(&mdia, b"hdlr")?;
    let handler_kind: [u8; 4] = handler.get(8..12)?.try_into().ok()?;
    if !matches!(&handler_kind, b"vide" | b"soun") {
        return None;
    }
    let minf = bmff_boxes(bmff_box(&mdia, b"minf")?)?;
    if !bmff_box(&minf, b"stbl").is_some_and(|value| valid_sample_table(value, fragmented)) {
        return None;
    }
    Some(handler_kind)
}

fn valid_movie_fragment(payload: &[u8]) -> bool {
    let Some(moof) = bmff_boxes(payload) else {
        return false;
    };
    if !bmff_box(&moof, b"mfhd").is_some_and(|value| value.len() >= 8) {
        return false;
    }
    moof.iter().filter(|item| &item.kind == b"traf").any(|traf| {
        let Some(children) = bmff_boxes(traf.payload) else {
            return false;
        };
        bmff_box(&children, b"tfhd").is_some_and(|value| value.len() >= 8)
            && children.iter().filter(|item| &item.kind == b"trun").any(|trun| {
                trun.payload.len() >= 8 && read_be_u32(&trun.payload[4..]).is_some_and(|count| count > 0)
            })
    })
}

/// Validate a complete playable ISO-BMFF skeleton. Merely having `ftyp`, an
/// empty `moov`, and one byte of `mdat` is intentionally insufficient.
fn iso_bmff_info(bytes: &[u8]) -> Option<BmffInfo> {
    let top = bmff_boxes(bytes)?;
    let ftyp = bmff_box(&top, b"ftyp")?;
    let brand: [u8; 4] = ftyp.get(..4)?.try_into().ok()?;
    if ftyp.len() < 8
        || !brand.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b' ')
        || brand.iter().all(|byte| *byte == 0)
    {
        return None;
    }
    let moov_payload = bmff_box(&top, b"moov")?;
    let moov = bmff_boxes(moov_payload)?;
    if !bmff_box(&moov, b"mvhd").is_some_and(|value| value.len() >= 8) {
        return None;
    }
    let has_mvex = bmff_box(&moov, b"mvex").is_some();
    let fragments = top.iter().filter(|item| &item.kind == b"moof").collect::<Vec<_>>();
    let fragmented = has_mvex || !fragments.is_empty();
    if fragmented && (!has_mvex || fragments.is_empty() || !fragments.iter().all(|item| valid_movie_fragment(item.payload))) {
        return None;
    }
    let mut has_audio = false;
    let mut has_video = false;
    let mut tracks = 0usize;
    for trak in moov.iter().filter(|item| &item.kind == b"trak") {
        match bmff_track_kind(trak.payload, fragmented)? {
            kind if &kind == b"soun" => has_audio = true,
            kind if &kind == b"vide" => has_video = true,
            _ => return None,
        }
        tracks += 1;
    }
    let has_media = top.iter().filter(|item| &item.kind == b"mdat").any(|item| {
        item.payload.len() >= 4 && item.payload.iter().any(|byte| *byte != 0)
    });
    (tracks > 0 && (has_audio || has_video) && has_media).then_some(BmffInfo {
        brand,
        has_audio,
        has_video,
    })
}

fn ebml_size(bytes: &[u8]) -> Option<(usize, usize, bool)> {
    let first = *bytes.first()?;
    let length = (first.leading_zeros() as usize).checked_add(1)?;
    if length > 8 || bytes.len() < length {
        return None;
    }
    let marker = 1_u8 << (8 - length);
    let mut value = usize::from(first & (marker - 1));
    for byte in &bytes[1..length] {
        value = value.checked_shl(8)?.checked_add(usize::from(*byte))?;
    }
    let unknown = value == ((1_u64 << (length * 7)) - 1) as usize;
    Some((value, length, unknown))
}

#[derive(Clone, Copy)]
struct EbmlElement<'a> {
    id: u32,
    payload: &'a [u8],
}

#[derive(Clone, Copy)]
struct WebmInfo {
    has_audio: bool,
    has_video: bool,
}

fn ebml_id(bytes: &[u8]) -> Option<(u32, usize)> {
    let first = *bytes.first()?;
    let length = (first.leading_zeros() as usize).checked_add(1)?;
    if length > 4 || bytes.len() < length {
        return None;
    }
    let mut id = 0u32;
    for byte in &bytes[..length] {
        id = id.checked_shl(8)?.checked_add(u32::from(*byte))?;
    }
    Some((id, length))
}

fn ebml_elements(bytes: &[u8]) -> Option<Vec<EbmlElement<'_>>> {
    let mut offset = 0usize;
    let mut elements = Vec::new();
    while offset < bytes.len() {
        if elements.len() >= MAX_CONTAINER_ELEMENTS {
            return None;
        }
        let (id, id_len) = ebml_id(&bytes[offset..])?;
        let size_offset = offset.checked_add(id_len)?;
        let (size, size_len, unknown) = ebml_size(&bytes[size_offset..])?;
        if unknown {
            return None;
        }
        let payload_start = size_offset.checked_add(size_len)?;
        let end = payload_start.checked_add(size)?;
        elements.push(EbmlElement {
            id,
            payload: bytes.get(payload_start..end)?,
        });
        offset = end;
    }
    (offset == bytes.len()).then_some(elements)
}

fn webm_track_type(payload: &[u8]) -> Option<u8> {
    let Some(elements) = ebml_elements(payload) else {
        return None;
    };
    let positive_uint = |id| {
        elements.iter().find(|item| item.id == id).is_some_and(|item| {
            !item.payload.is_empty()
                && item.payload.len() <= 8
                && item.payload.iter().fold(0u64, |value, byte| (value << 8) | u64::from(*byte)) > 0
        })
    };
    let track_type = elements
        .iter()
        .find(|item| item.id == 0x83)
        .and_then(|item| (item.payload.len() == 1 && matches!(item.payload[0], 1 | 2)).then_some(item.payload[0]));
    let valid_codec = elements.iter().find(|item| item.id == 0x86).is_some_and(|item| {
        !item.payload.is_empty()
            && item.payload.len() <= 128
            && item.payload.iter().all(u8::is_ascii_graphic)
    });
    (positive_uint(0xd7) && valid_codec).then_some(track_type?)
}

fn valid_webm_block(payload: &[u8]) -> bool {
    let Some((track, track_len, unknown)) = ebml_size(payload) else {
        return false;
    };
    if unknown || track == 0 {
        return false;
    }
    let frame_start = track_len.saturating_add(3);
    payload.len() > frame_start && payload[frame_start..].iter().any(|byte| *byte != 0)
}

fn webm_info(bytes: &[u8]) -> Option<WebmInfo> {
    let Some(top) = ebml_elements(bytes) else {
        return None;
    };
    if top.len() != 2 || top[0].id != 0x1a45dfa3 || top[1].id != 0x18538067 {
        return None;
    }
    let Some(header) = ebml_elements(top[0].payload) else {
        return None;
    };
    if !header
        .iter()
        .any(|item| item.id == 0x4282 && item.payload == b"webm")
    {
        return None;
    }
    let Some(segment) = ebml_elements(top[1].payload) else {
        return None;
    };
    let valid_info = segment.iter().find(|item| item.id == 0x1549a966).is_some_and(|item| {
        !item.payload.is_empty() && ebml_elements(item.payload).is_some_and(|children| !children.is_empty())
    });
    let track_types = segment
        .iter()
        .find(|item| item.id == 0x1654ae6b)
        .and_then(|item| ebml_elements(item.payload))?
        .into_iter()
        .filter(|entry| entry.id == 0xae)
        .map(|entry| webm_track_type(entry.payload))
        .collect::<Option<Vec<_>>>()?;
    let has_video = track_types.contains(&1);
    let has_audio = track_types.contains(&2);
    let valid_cluster = segment.iter().filter(|item| item.id == 0x1f43b675).any(|item| {
        ebml_elements(item.payload).is_some_and(|cluster| {
            cluster.iter().any(|child| {
                (child.id == 0xa3 && valid_webm_block(child.payload))
                    || (child.id == 0xa0
                        && ebml_elements(child.payload).is_some_and(|group| {
                            group
                                .iter()
                                .any(|block| block.id == 0xa1 && valid_webm_block(block.payload))
                        }))
            })
        })
    });
    (valid_info && !track_types.is_empty() && valid_cluster).then_some(WebmInfo {
        has_audio,
        has_video,
    })
}

fn valid_wav(bytes: &[u8]) -> bool {
    if !bytes.starts_with(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return false;
    }
    let Some(riff_len) = read_le_u32(&bytes[4..]).and_then(|size| usize::try_from(size).ok()) else {
        return false;
    };
    let Some(end) = riff_len.checked_add(8) else {
        return false;
    };
    if end != bytes.len() || end < 12 {
        return false;
    }
    let (mut offset, mut block_align, mut has_data) = (12usize, None, false);
    while offset + 8 <= end {
        let id = &bytes[offset..offset + 4];
        let Some(size) = read_le_u32(&bytes[offset + 4..]).and_then(|size| usize::try_from(size).ok()) else {
            return false;
        };
        let Some(chunk_end) = offset.checked_add(8).and_then(|value| value.checked_add(size)) else {
            return false;
        };
        if chunk_end > end {
            return false;
        }
        if id == b"fmt " && size >= 16 && block_align.is_none() {
            let Some(format) = bytes.get(offset + 8..offset + 8 + 16) else {
                return false;
            };
            let codec = u16::from_le_bytes(format[..2].try_into().expect("WAV codec exists"));
            let channels = u16::from_le_bytes(format[2..4].try_into().expect("WAV channels exist"));
            let sample_rate = u32::from_le_bytes(format[4..8].try_into().expect("WAV rate exists"));
            let byte_rate = u32::from_le_bytes(format[8..12].try_into().expect("WAV byte rate exists"));
            let align = u16::from_le_bytes(format[12..14].try_into().expect("WAV alignment exists"));
            let bits = u16::from_le_bytes(format[14..16].try_into().expect("WAV sample size exists"));
            let expected_align = u32::from(channels).checked_mul(u32::from(bits).div_ceil(8));
            if !matches!(codec, 1 | 3 | 6 | 7 | 0xfffe)
                || channels == 0
                || channels > 32
                || !(1..=768_000).contains(&sample_rate)
                || bits == 0
                || bits > 64
                || align == 0
                || expected_align != Some(u32::from(align))
                || sample_rate.checked_mul(u32::from(align)) != Some(byte_rate)
            {
                return false;
            }
            block_align = Some(usize::from(align));
        } else if id == b"fmt " {
            return false;
        }
        if id == b"data" {
            let Some(align) = block_align else {
                return false;
            };
            if has_data || size == 0 || size % align != 0 {
                return false;
            }
            has_data = true;
        }
        let Some(next) = chunk_end.checked_add(size & 1) else {
            return false;
        };
        if next > end || (size & 1 == 1 && bytes.get(chunk_end) != Some(&0)) {
            return false;
        }
        offset = next;
    }
    block_align.is_some() && has_data && offset == end
}

#[derive(Clone, Copy)]
enum OggCodec {
    Opus,
    Vorbis,
    Speex,
    Flac,
}

fn ogg_codec_header(packet: &[u8]) -> Option<OggCodec> {
    if packet.starts_with(b"OpusHead")
        && packet.len() >= 19
        && (1..=15).contains(&packet[8])
        && packet[9] > 0
    {
        Some(OggCodec::Opus)
    } else if packet.len() >= 30
        && packet[0] == 1
        && packet.get(1..7) == Some(b"vorbis")
        && packet[7..11] == [0, 0, 0, 0]
        && packet[11] > 0
        && u32::from_le_bytes(packet[12..16].try_into().ok()?) > 0
        && packet[28] & 0x0f >= 6
        && packet[28] >> 4 >= packet[28] & 0x0f
        && packet[28] >> 4 <= 13
        && packet[29] & 1 == 1
    {
        Some(OggCodec::Vorbis)
    } else if packet.starts_with(b"Speex   ") && packet.len() >= 80 {
        Some(OggCodec::Speex)
    } else if packet.starts_with(b"\x7fFLAC") && packet.len() >= 9 {
        Some(OggCodec::Flac)
    } else {
        None
    }
}

fn ogg_packet_is_audio(codec: OggCodec, packet_index: usize, packet: &[u8]) -> bool {
    if packet.is_empty() || packet.iter().all(|byte| *byte == 0) {
        return false;
    }
    match codec {
        OggCodec::Opus => !packet.starts_with(b"OpusHead") && !packet.starts_with(b"OpusTags"),
        OggCodec::Vorbis => {
            !(packet.len() >= 7
                && matches!(packet[0], 1 | 3 | 5)
                && packet.get(1..7) == Some(b"vorbis"))
                && packet[0] & 1 == 0
        }
        // Speex has an identification packet followed by a comment packet.
        OggCodec::Speex => packet_index >= 2,
        OggCodec::Flac => {
            packet.len() >= 2 && packet[0] == 0xff && packet[1] & 0xfe == 0xf8
        }
    }
}

fn valid_ogg(bytes: &[u8]) -> bool {
    let (mut offset, mut pages) = (0usize, 0usize);
    let mut serial = None;
    let mut expected_sequence = 0u32;
    let mut packet = Vec::new();
    let mut codec = None;
    let mut packet_index = 0usize;
    let mut saw_audio = false;
    let mut saw_end = false;
    while offset < bytes.len() {
        let Some(header) = bytes.get(offset..offset + 27) else {
            return false;
        };
        if &header[..4] != b"OggS" || header[4] != 0 {
            return false;
        }
        let flags = header[5];
        let segment_count = header[26] as usize;
        if segment_count == 0
            || (pages == 0 && (flags & 0x02 == 0 || flags & 0x01 != 0))
            || (pages > 0 && flags & 0x02 != 0)
            || (flags & 0x01 != 0) != !packet.is_empty()
            || saw_end
        {
            return false;
        }
        let page_serial = u32::from_le_bytes(header[14..18].try_into().expect("Ogg serial exists"));
        let sequence = u32::from_le_bytes(header[18..22].try_into().expect("Ogg sequence exists"));
        if serial.is_some_and(|known| known != page_serial) || sequence != expected_sequence {
            return false;
        }
        serial.get_or_insert(page_serial);
        expected_sequence = expected_sequence.wrapping_add(1);
        let Some(lacing) = bytes.get(offset + 27..offset + 27 + segment_count) else {
            return false;
        };
        let payload_len: usize = lacing.iter().map(|value| *value as usize).sum();
        let Some(next) = offset
            .checked_add(27 + segment_count)
            .and_then(|value| value.checked_add(payload_len))
        else {
            return false;
        };
        if next > bytes.len() {
            return false;
        }
        let page = &bytes[offset..next];
        let stored_crc = u32::from_le_bytes(page[22..26].try_into().expect("Ogg CRC field exists"));
        if ogg_crc(page) != stored_crc {
            return false;
        }
        let mut payload_offset = offset + 27 + segment_count;
        for lace in lacing {
            let end = payload_offset + usize::from(*lace);
            if packet.len().saturating_add(usize::from(*lace)) > MAX_OGG_PACKET_BYTES {
                return false;
            }
            packet.extend_from_slice(&bytes[payload_offset..end]);
            payload_offset = end;
            if *lace < 255 {
                if let Some(known) = codec {
                    saw_audio |= ogg_packet_is_audio(known, packet_index, &packet);
                } else {
                    codec = ogg_codec_header(&packet);
                    if codec.is_none() {
                        return false;
                    }
                }
                packet_index += 1;
                packet.clear();
            }
        }
        saw_end = flags & 0x04 != 0;
        if saw_end {
            let granule = u64::from_le_bytes(header[6..14].try_into().expect("Ogg granule exists"));
            if granule == 0 || granule == u64::MAX || next != bytes.len() {
                return false;
            }
        }
        offset = next;
        pages += 1;
    }
    pages > 0 && codec.is_some() && saw_audio && saw_end && packet.is_empty()
}

fn ogg_crc(bytes: &[u8]) -> u32 {
    let mut crc = 0_u32;
    for (index, byte) in bytes.iter().copied().enumerate() {
        let byte = if (22..26).contains(&index) { 0 } else { byte };
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

struct BitReader<'a> {
    bytes: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit: 0 }
    }

    fn read(&mut self, bits: usize) -> Option<u64> {
        if bits > 64 || self.bit.checked_add(bits)? > self.bytes.len().checked_mul(8)? {
            return None;
        }
        let mut value = 0u64;
        for _ in 0..bits {
            value = (value << 1)
                | u64::from((self.bytes[self.bit / 8] >> (7 - self.bit % 8)) & 1);
            self.bit += 1;
        }
        Some(value)
    }

    fn skip(&mut self, bits: usize) -> Option<()> {
        self.bit = self.bit.checked_add(bits)?;
        (self.bit <= self.bytes.len().checked_mul(8)?).then_some(())
    }

    fn unary(&mut self) -> Option<usize> {
        let mut zeros = 0usize;
        while self.read(1)? == 0 {
            zeros = zeros.checked_add(1)?;
            if zeros > 32 * 1024 * 1024 {
                return None;
            }
        }
        Some(zeros)
    }

    fn align_zero(&mut self) -> Option<()> {
        while self.bit % 8 != 0 {
            if self.read(1)? != 0 {
                return None;
            }
        }
        Some(())
    }

    fn byte_offset(&self) -> usize {
        self.bit / 8
    }
}

fn flac_crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    for byte in bytes {
        crc ^= *byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
        }
    }
    crc
}

fn flac_utf8_number(bytes: &[u8], offset: &mut usize) -> Option<u64> {
    let first = *bytes.get(*offset)?;
    let (length, mut value, minimum) = if first & 0x80 == 0 {
        (1usize, u64::from(first), 0u64)
    } else {
        let leading = first.leading_ones() as usize;
        if !(2..=6).contains(&leading) {
            return None;
        }
        (leading, u64::from(first & (0x7f >> leading)), 1u64 << (5 * leading - 4))
    };
    for index in 1..length {
        let next = *bytes.get(offset.checked_add(index)?)?;
        if next & 0xc0 != 0x80 {
            return None;
        }
        value = value.checked_shl(6)?.checked_add(u64::from(next & 0x3f))?;
    }
    if length > 1 && value < minimum {
        return None;
    }
    *offset = offset.checked_add(length)?;
    Some(value)
}

fn flac_residual(reader: &mut BitReader<'_>, block_size: usize, predictor_order: usize) -> Option<()> {
    let method = reader.read(2)? as usize;
    if method > 1 {
        return None;
    }
    let parameter_bits = if method == 0 { 4 } else { 5 };
    let escape = (1usize << parameter_bits) - 1;
    let partition_order = reader.read(4)? as usize;
    let partitions = 1usize.checked_shl(u32::try_from(partition_order).ok()?)?;
    if partitions == 0 || block_size % partitions != 0 {
        return None;
    }
    let partition_samples = block_size / partitions;
    for partition in 0..partitions {
        let samples = if partition == 0 {
            partition_samples.checked_sub(predictor_order)?
        } else {
            partition_samples
        };
        let parameter = reader.read(parameter_bits)? as usize;
        if parameter == escape {
            let raw_bits = reader.read(5)? as usize;
            reader.skip(samples.checked_mul(raw_bits)?)?;
        } else {
            for _ in 0..samples {
                reader.unary()?;
                reader.skip(parameter)?;
            }
        }
    }
    Some(())
}

fn flac_subframe(reader: &mut BitReader<'_>, block_size: usize, bits_per_sample: usize) -> Option<()> {
    if reader.read(1)? != 0 {
        return None;
    }
    let kind = reader.read(6)? as usize;
    let wasted = if reader.read(1)? == 1 {
        reader.unary()?.checked_add(1)?
    } else {
        0
    };
    let sample_bits = bits_per_sample.checked_sub(wasted)?;
    if sample_bits == 0 || sample_bits > 64 {
        return None;
    }
    match kind {
        0 => reader.skip(sample_bits),
        1 => reader.skip(block_size.checked_mul(sample_bits)?),
        8..=12 => {
            let order = kind - 8;
            reader.skip(order.checked_mul(sample_bits)?)?;
            flac_residual(reader, block_size, order)
        }
        32..=63 => {
            let order = (kind & 31) + 1;
            reader.skip(order.checked_mul(sample_bits)?)?;
            let precision = reader.read(4)? as usize;
            if precision == 15 {
                return None;
            }
            reader.skip(5)?; // signed LPC shift
            reader.skip(order.checked_mul(precision + 1)?)?;
            flac_residual(reader, block_size, order)
        }
        _ => None,
    }
}

fn parse_flac_frame(
    bytes: &[u8],
    stream_bits_per_sample: usize,
    min_block_size: usize,
    max_block_size: usize,
) -> Option<(usize, usize)> {
    if bytes.get(0) != Some(&0xff)
        || bytes.get(1).is_none_or(|byte| byte & 0xfe != 0xf8)
        || bytes.get(1).is_some_and(|byte| byte & 0x02 != 0)
    {
        return None;
    }
    let block_code = usize::from(*bytes.get(2)? >> 4);
    let sample_rate_code = usize::from(*bytes.get(2)? & 0x0f);
    let channel_assignment = usize::from(*bytes.get(3)? >> 4);
    let sample_size_code = usize::from((*bytes.get(3)? >> 1) & 0x07);
    if block_code == 0 || sample_rate_code == 15 || channel_assignment > 10 || bytes[3] & 1 != 0 {
        return None;
    }
    let mut offset = 4usize;
    flac_utf8_number(bytes, &mut offset)?;
    let block_size = match block_code {
        1 => 192,
        2..=5 => 576usize.checked_shl(u32::try_from(block_code - 2).ok()?)?,
        6 => usize::from(*bytes.get(offset)?).checked_add(1).map(|value| {
            offset += 1;
            value
        })?,
        7 => {
            let value = usize::from(u16::from_be_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?)) + 1;
            offset += 2;
            value
        }
        8..=15 => 256usize.checked_shl(u32::try_from(block_code - 8).ok()?)?,
        _ => return None,
    };
    match sample_rate_code {
        12 => {
            if *bytes.get(offset)? == 0 {
                return None;
            }
            offset += 1;
        }
        13 | 14 => {
            if u16::from_be_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?) == 0 {
                return None;
            }
            offset += 2;
        }
        _ => {}
    }
    let bits_per_sample = match sample_size_code {
        0 => stream_bits_per_sample,
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        _ => return None,
    };
    if block_size < min_block_size || block_size > max_block_size || flac_crc8(bytes.get(..offset)?) != *bytes.get(offset)? {
        return None;
    }
    let frame_data = offset + 1;
    let channels = if channel_assignment <= 7 { channel_assignment + 1 } else { 2 };
    let mut reader = BitReader::new(bytes.get(frame_data..)?);
    for channel in 0..channels {
        let extra = usize::from(
            (channel_assignment == 8 && channel == 1)
                || (channel_assignment == 9 && channel == 0)
                || (channel_assignment == 10 && channel == 1),
        );
        flac_subframe(&mut reader, block_size, bits_per_sample.checked_add(extra)?)?;
    }
    reader.align_zero()?;
    let crc_offset = frame_data.checked_add(reader.byte_offset())?;
    let stored = u16::from_be_bytes(bytes.get(crc_offset..crc_offset + 2)?.try_into().ok()?);
    let calculated = bytes[..crc_offset]
        .iter()
        .fold(0u16, |crc, byte| flac_crc16_update(crc, *byte));
    (calculated == stored).then_some((crc_offset + 2, block_size))
}

fn valid_flac(bytes: &[u8]) -> bool {
    if !bytes.starts_with(b"fLaC") {
        return false;
    }
    let (mut offset, mut first, mut final_block) = (4usize, true, false);
    let mut stream = None;
    while !final_block {
        let Some(header) = bytes.get(offset..offset + 4) else {
            return false;
        };
        final_block = header[0] & 0x80 != 0;
        let block_type = header[0] & 0x7f;
        let len = ((header[1] as usize) << 16) | ((header[2] as usize) << 8) | header[3] as usize;
        if block_type == 127 || (first && (block_type != 0 || len != 34)) {
            return false;
        }
        let Some(next) = offset.checked_add(4).and_then(|value| value.checked_add(len)) else {
            return false;
        };
        let Some(payload) = bytes.get(offset + 4..next) else {
            return false;
        };
        if first {
            let min_block = usize::from(u16::from_be_bytes(payload[0..2].try_into().expect("STREAMINFO block size")));
            let max_block = usize::from(u16::from_be_bytes(payload[2..4].try_into().expect("STREAMINFO block size")));
            let min_frame = ((payload[4] as usize) << 16) | ((payload[5] as usize) << 8) | payload[6] as usize;
            let max_frame = ((payload[7] as usize) << 16) | ((payload[8] as usize) << 8) | payload[9] as usize;
            let word = u64::from_be_bytes(payload[10..18].try_into().expect("STREAMINFO stream fields"));
            let sample_rate = (word >> 44) & 0x0f_ffff;
            let channels = ((word >> 41) & 0x07) + 1;
            let bits_per_sample = ((word >> 36) & 0x1f) + 1;
            let total_samples = word & 0x0f_ffff_ffff;
            if min_block < 16
                || max_block < min_block
                || (min_frame > 0 && max_frame > 0 && max_frame < min_frame)
                || sample_rate == 0
                || channels > 8
                || !(4..=32).contains(&bits_per_sample)
                || total_samples == 0
            {
                return false;
            }
            stream = Some((min_block, max_block, bits_per_sample as usize, total_samples));
        }
        first = false;
        offset = next;
    }
    let Some((min_block, max_block, bits_per_sample, total_samples)) = stream else {
        return false;
    };
    let mut frames = 0usize;
    let mut decoded_samples = 0u64;
    while offset < bytes.len() {
        let Some((consumed, block_size)) = parse_flac_frame(&bytes[offset..], bits_per_sample, min_block, max_block) else {
            return false;
        };
        if consumed == 0 {
            return false;
        }
        offset += consumed;
        decoded_samples = match decoded_samples.checked_add(block_size as u64) {
            Some(value) => value,
            None => return false,
        };
        frames += 1;
    }
    frames > 0 && decoded_samples == total_samples
}

fn flac_crc16_update(mut crc: u16, byte: u8) -> u16 {
    crc ^= u16::from(byte) << 8;
    for _ in 0..8 {
        crc = if crc & 0x8000 != 0 {
            (crc << 1) ^ 0x8005
        } else {
            crc << 1
        };
    }
    crc
}

fn mp3_frame_len(header: &[u8]) -> Option<usize> {
    if header.len() < 4 || header[0] != 0xff || (header[1] & 0xe0) != 0xe0 {
        return None;
    }
    let version = (header[1] >> 3) & 0x03;
    let layer = (header[1] >> 1) & 0x03;
    let bitrate_index = (header[2] >> 4) as usize;
    let sample_index = ((header[2] >> 2) & 0x03) as usize;
    if version == 1 || layer == 0 || !(1..15).contains(&bitrate_index) || sample_index == 3 {
        return None;
    }
    const MPEG1_LAYER1: [usize; 16] = [
        0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0,
    ];
    const MPEG1_LAYER2: [usize; 16] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
    ];
    const MPEG1_LAYER3: [usize; 16] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
    ];
    const MPEG2_LAYER1: [usize; 16] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0,
    ];
    const MPEG2_OTHER: [usize; 16] = [
        0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
    ];
    let bitrate_kbps = match (version == 3, layer) {
        (true, 3) => MPEG1_LAYER1[bitrate_index],
        (true, 2) => MPEG1_LAYER2[bitrate_index],
        (true, 1) => MPEG1_LAYER3[bitrate_index],
        (false, 3) => MPEG2_LAYER1[bitrate_index],
        (false, _) => MPEG2_OTHER[bitrate_index],
        _ => return None,
    };
    let base_sample_rate = [44_100usize, 48_000, 32_000][sample_index];
    let sample_rate = match version {
        3 => base_sample_rate,
        2 => base_sample_rate / 2,
        0 => base_sample_rate / 4,
        _ => return None,
    };
    let padding = ((header[2] >> 1) & 1) as usize;
    let bitrate = bitrate_kbps.checked_mul(1_000)?;
    let frame_len = if layer == 3 {
        (12 * bitrate / sample_rate + padding) * 4
    } else if layer == 1 && version != 3 {
        72 * bitrate / sample_rate + padding
    } else {
        144 * bitrate / sample_rate + padding
    };
    (frame_len >= 4).then_some(frame_len)
}

fn valid_mp3(bytes: &[u8]) -> bool {
    let mut offset = 0usize;
    if bytes.starts_with(b"ID3") {
        let Some(header) = bytes.get(..10) else {
            return false;
        };
        let size = &header[6..10];
        if size.iter().any(|byte| byte & 0x80 != 0) {
            return false;
        }
        let tag_len = size.iter().fold(0usize, |value, byte| (value << 7) | *byte as usize);
        let footer_len = if header[3] == 4 && header[5] & 0x10 != 0 {
            10
        } else {
            0
        };
        let Some(next) = 10usize.checked_add(tag_len).and_then(|value| value.checked_add(footer_len)) else {
            return false;
        };
        if next > bytes.len() {
            return false;
        }
        offset = next;
    }

    let mut frames = 0usize;
    let mut stream_signature = None;
    while offset < bytes.len() {
        if bytes.len() - offset == 128 && bytes.get(offset..offset + 3) == Some(b"TAG") {
            offset = bytes.len();
            break;
        }
        let Some(frame_len) = mp3_frame_len(&bytes[offset..]) else {
            return false;
        };
        let header = &bytes[offset..offset + 4];
        let signature = (header[1] & 0x1e, header[2] & 0x0c, header[3] & 0xc0);
        if stream_signature.is_some_and(|known| known != signature) {
            return false;
        }
        stream_signature.get_or_insert(signature);
        let Some(next) = offset.checked_add(frame_len) else {
            return false;
        };
        if next > bytes.len() {
            return false;
        }
        let payload_start = offset + if header[1] & 1 == 0 { 6 } else { 4 };
        if payload_start >= next || bytes[payload_start..next].iter().all(|byte| *byte == 0) {
            return false;
        }
        offset = next;
        frames += 1;
    }
    frames >= 2 && offset == bytes.len()
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|window| window == needle)
}

fn rfind_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty())
        .then(|| haystack.windows(needle.len()).rposition(|window| window == needle))
        .flatten()
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty())
        .then(|| haystack.windows(needle.len()).position(|window| window == needle))
        .flatten()
}

const MAX_PDF_DICTIONARY_BYTES: usize = 1024 * 1024;
const MAX_PDF_XREF_STREAM_BYTES: usize = 1024 * 1024;
const MAX_PDF_SYNTAX_DEPTH: usize = 32;

type PdfObjectOffsets = std::collections::HashMap<(u32, u16), usize>;

struct PdfXref<'a> {
    objects: PdfObjectOffsets,
    trailer: &'a [u8],
}

fn pdf_is_whitespace(byte: u8) -> bool {
    byte == 0 || byte.is_ascii_whitespace()
}

fn pdf_is_delimiter(byte: u8) -> bool {
    pdf_is_whitespace(byte) || b"()<>[]{}/%".contains(&byte)
}

fn pdf_skip_whitespace(bytes: &[u8], offset: &mut usize) {
    while bytes.get(*offset).is_some_and(|byte| pdf_is_whitespace(*byte)) {
        *offset += 1;
    }
}

fn pdf_skip_space_and_comments(bytes: &[u8], offset: &mut usize) {
    loop {
        pdf_skip_whitespace(bytes, offset);
        if bytes.get(*offset) != Some(&b'%') {
            return;
        }
        while bytes.get(*offset).is_some_and(|byte| !matches!(byte, b'\r' | b'\n')) {
            *offset += 1;
        }
    }
}

fn pdf_uint(bytes: &[u8], offset: &mut usize) -> Option<u32> {
    pdf_skip_space_and_comments(bytes, offset);
    let start = *offset;
    while bytes.get(*offset).is_some_and(u8::is_ascii_digit) {
        *offset += 1;
    }
    if *offset == start || bytes.get(*offset).is_some_and(|byte| !pdf_is_delimiter(*byte)) {
        return None;
    }
    std::str::from_utf8(&bytes[start..*offset]).ok()?.parse().ok()
}

fn pdf_consume_keyword(bytes: &[u8], offset: &mut usize, keyword: &[u8]) -> bool {
    pdf_skip_space_and_comments(bytes, offset);
    let Some(end) = offset.checked_add(keyword.len()) else {
        return false;
    };
    if bytes.get(*offset..end) != Some(keyword)
        || bytes.get(end).is_some_and(|byte| !pdf_is_delimiter(*byte))
    {
        return false;
    }
    *offset = end;
    true
}

fn pdf_parse_name<'a>(bytes: &'a [u8], offset: &mut usize, limit: usize) -> Option<&'a [u8]> {
    if *offset >= limit || bytes.get(*offset) != Some(&b'/') {
        return None;
    }
    let start = *offset;
    *offset += 1;
    while *offset < limit && bytes.get(*offset).is_some_and(|byte| !pdf_is_delimiter(*byte)) {
        *offset += 1;
    }
    (*offset > start + 1).then(|| &bytes[start..*offset])
}

fn pdf_skip_literal_string(bytes: &[u8], offset: &mut usize, limit: usize) -> bool {
    if bytes.get(*offset) != Some(&b'(') {
        return false;
    }
    *offset += 1;
    let mut depth = 1usize;
    while *offset < limit {
        match bytes[*offset] {
            b'\\' => {
                *offset += 1;
                if *offset >= limit {
                    return false;
                }
                if bytes[*offset] == b'\r' {
                    *offset += 1;
                    if *offset < limit && bytes[*offset] == b'\n' {
                        *offset += 1;
                    }
                } else {
                    *offset += 1;
                }
            }
            b'(' => {
                depth += 1;
                if depth > MAX_PDF_SYNTAX_DEPTH {
                    return false;
                }
                *offset += 1;
            }
            b')' => {
                depth -= 1;
                *offset += 1;
                if depth == 0 {
                    return true;
                }
            }
            _ => *offset += 1,
        }
    }
    false
}

fn pdf_skip_hex_string(bytes: &[u8], offset: &mut usize, limit: usize) -> bool {
    if bytes.get(*offset) != Some(&b'<') || bytes.get(*offset + 1) == Some(&b'<') {
        return false;
    }
    *offset += 1;
    while *offset < limit {
        let byte = bytes[*offset];
        *offset += 1;
        if byte == b'>' {
            return true;
        }
        if !byte.is_ascii_hexdigit() && !pdf_is_whitespace(byte) {
            return false;
        }
    }
    false
}

fn pdf_skip_number(bytes: &[u8], offset: &mut usize, limit: usize) -> Option<bool> {
    let start = *offset;
    if *offset < limit && matches!(bytes[*offset], b'+' | b'-') {
        *offset += 1;
    }
    let mut digits = 0usize;
    while *offset < limit && bytes[*offset].is_ascii_digit() {
        digits += 1;
        *offset += 1;
    }
    let mut integer = true;
    if *offset < limit && bytes[*offset] == b'.' {
        integer = false;
        *offset += 1;
        while *offset < limit && bytes[*offset].is_ascii_digit() {
            digits += 1;
            *offset += 1;
        }
    }
    if digits == 0
        || *offset == start
        || bytes
            .get(*offset)
            .is_some_and(|byte| !pdf_is_delimiter(*byte))
    {
        return None;
    }
    Some(integer)
}

fn pdf_skip_value(bytes: &[u8], offset: &mut usize, depth: usize, limit: usize) -> bool {
    if depth > MAX_PDF_SYNTAX_DEPTH || *offset > limit || limit > bytes.len() {
        return false;
    }
    pdf_skip_space_and_comments(bytes, offset);
    if *offset >= limit {
        return false;
    }
    match bytes[*offset] {
        b'/' => pdf_parse_name(bytes, offset, limit).is_some(),
        b'(' => pdf_skip_literal_string(bytes, offset, limit),
        b'<' if bytes.get(*offset + 1) == Some(&b'<') => {
            *offset += 2;
            let mut entries = 0usize;
            loop {
                pdf_skip_space_and_comments(bytes, offset);
                if *offset + 2 <= limit && bytes.get(*offset..*offset + 2) == Some(b">>") {
                    *offset += 2;
                    return true;
                }
                if entries >= MAX_CONTAINER_ELEMENTS
                    || pdf_parse_name(bytes, offset, limit).is_none()
                    || !pdf_skip_value(bytes, offset, depth + 1, limit)
                {
                    return false;
                }
                entries += 1;
            }
        }
        b'<' => pdf_skip_hex_string(bytes, offset, limit),
        b'[' => {
            *offset += 1;
            let mut entries = 0usize;
            loop {
                pdf_skip_space_and_comments(bytes, offset);
                if *offset < limit && bytes[*offset] == b']' {
                    *offset += 1;
                    return true;
                }
                if entries >= MAX_CONTAINER_ELEMENTS
                    || !pdf_skip_value(bytes, offset, depth + 1, limit)
                {
                    return false;
                }
                entries += 1;
            }
        }
        b't' | b'f' | b'n' => {
            let keyword = match bytes[*offset] {
                b't' => b"true".as_slice(),
                b'f' => b"false".as_slice(),
                _ => b"null".as_slice(),
            };
            let end = match offset.checked_add(keyword.len()) {
                Some(end) if end <= limit => end,
                _ => return false,
            };
            if bytes.get(*offset..end) != Some(keyword)
                || bytes.get(end).is_some_and(|byte| !pdf_is_delimiter(*byte))
            {
                return false;
            }
            *offset = end;
            true
        }
        b'+' | b'-' | b'.' | b'0'..=b'9' => {
            let first_start = *offset;
            let Some(first_is_integer) = pdf_skip_number(bytes, offset, limit) else {
                return false;
            };
            let first_end = *offset;
            if first_is_integer && !matches!(bytes[first_start], b'-' | b'+') {
                let mut reference_end = first_end;
                pdf_skip_space_and_comments(bytes, &mut reference_end);
                let second_start = reference_end;
                if reference_end < limit && bytes[reference_end].is_ascii_digit() {
                    if pdf_skip_number(bytes, &mut reference_end, limit) == Some(true)
                        && !matches!(bytes[second_start], b'-' | b'+')
                    {
                        let mut r = reference_end;
                        if pdf_consume_keyword(&bytes[..limit], &mut r, b"R") {
                            *offset = r;
                        }
                    }
                }
            }
            true
        }
        _ => false,
    }
}

fn pdf_dictionary_at<'a>(bytes: &'a [u8], offset: &mut usize) -> Option<&'a [u8]> {
    pdf_skip_space_and_comments(bytes, offset);
    let open = *offset;
    if bytes.get(open..open + 2) != Some(b"<<") {
        return None;
    }
    let limit = bytes
        .len()
        .min(open.checked_add(MAX_PDF_DICTIONARY_BYTES)?.checked_add(4)?);
    if !pdf_skip_value(&bytes[..limit], offset, 0, limit) || *offset < open + 4 {
        return None;
    }
    bytes.get(open + 2..*offset - 2)
}

fn pdf_value_after<'a>(dictionary: &'a [u8], key: &[u8]) -> Result<Option<&'a [u8]>, ()> {
    let mut offset = 0usize;
    let mut found = None;
    let mut entries = 0usize;
    loop {
        pdf_skip_space_and_comments(dictionary, &mut offset);
        if offset == dictionary.len() {
            return Ok(found);
        }
        if offset > dictionary.len() || entries >= MAX_CONTAINER_ELEMENTS {
            return Err(());
        }
        let name = pdf_parse_name(dictionary, &mut offset, dictionary.len()).ok_or(())?;
        pdf_skip_space_and_comments(dictionary, &mut offset);
        let value_start = offset;
        if !pdf_skip_value(dictionary, &mut offset, 0, dictionary.len()) {
            return Err(());
        }
        if name == key {
            if found.is_some() {
                return Err(());
            }
            found = Some(&dictionary[value_start..offset]);
        }
        entries += 1;
    }
}

fn pdf_value_uint(value: &[u8]) -> Option<u32> {
    let mut offset = 0usize;
    let output = pdf_uint(value, &mut offset)?;
    pdf_skip_space_and_comments(value, &mut offset);
    (offset == value.len()).then_some(output)
}

fn pdf_value_ref(value: &[u8]) -> Option<(u32, u16)> {
    let mut offset = 0usize;
    let object = pdf_uint(value, &mut offset)?;
    let generation = u16::try_from(pdf_uint(value, &mut offset)?).ok()?;
    if !pdf_consume_keyword(value, &mut offset, b"R") {
        return None;
    }
    pdf_skip_space_and_comments(value, &mut offset);
    (offset == value.len()).then_some((object, generation))
}

fn pdf_value_name<'a>(value: &'a [u8]) -> Option<&'a [u8]> {
    let mut offset = 0usize;
    pdf_skip_space_and_comments(value, &mut offset);
    let name = pdf_parse_name(value, &mut offset, value.len())?;
    pdf_skip_space_and_comments(value, &mut offset);
    (offset == value.len()).then_some(name)
}

fn pdf_value_uint_array(value: &[u8], max_items: usize) -> Option<Vec<u32>> {
    let mut offset = 0usize;
    pdf_skip_space_and_comments(value, &mut offset);
    if value.get(offset) != Some(&b'[') {
        return None;
    }
    offset += 1;
    let mut output = Vec::new();
    loop {
        pdf_skip_space_and_comments(value, &mut offset);
        if value.get(offset) == Some(&b']') {
            offset += 1;
            pdf_skip_space_and_comments(value, &mut offset);
            return (offset == value.len()).then_some(output);
        }
        if output.len() >= max_items {
            return None;
        }
        output.push(pdf_uint(value, &mut offset)?);
    }
}

fn pdf_ref_after(dictionary: &[u8], key: &[u8]) -> Option<(u32, u16)> {
    pdf_value_ref(pdf_value_after(dictionary, key).ok()??)
}

fn pdf_int_after(dictionary: &[u8], key: &[u8]) -> Option<u32> {
    pdf_value_uint(pdf_value_after(dictionary, key).ok()??)
}

fn pdf_object_has_type(dictionary: &[u8], expected: &[u8]) -> bool {
    pdf_value_after(dictionary, b"/Type")
        .ok()
        .flatten()
        .and_then(pdf_value_name)
        == Some(expected)
}

fn pdf_kids(dictionary: &[u8]) -> Option<Vec<(u32, u16)>> {
    let value = pdf_value_after(dictionary, b"/Kids").ok()??;
    let mut offset = 0usize;
    pdf_skip_space_and_comments(value, &mut offset);
    if value.get(offset) != Some(&b'[') {
        return None;
    }
    offset += 1;
    let mut output = Vec::new();
    loop {
        pdf_skip_space_and_comments(value, &mut offset);
        if value.get(offset) == Some(&b']') {
            offset += 1;
            pdf_skip_space_and_comments(value, &mut offset);
            return (offset == value.len() && !output.is_empty()).then_some(output);
        }
        if output.len() >= MAX_CONTAINER_ELEMENTS {
            return None;
        }
        let object = pdf_uint(value, &mut offset)?;
        let generation = u16::try_from(pdf_uint(value, &mut offset)?).ok()?;
        if !pdf_consume_keyword(value, &mut offset, b"R") {
            return None;
        }
        output.push((object, generation));
    }
}

fn pdf_indirect_header_at(
    document: &[u8],
    offset: usize,
) -> Option<((u32, u16), usize)> {
    if !document.get(offset).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let mut cursor = offset;
    let object = pdf_uint(document, &mut cursor)?;
    let generation = u16::try_from(pdf_uint(document, &mut cursor)?).ok()?;
    if !pdf_consume_keyword(document, &mut cursor, b"obj") {
        return None;
    }
    Some(((object, generation), cursor))
}

fn pdf_dictionary_object_at<'a>(
    document: &'a [u8],
    objects: &PdfObjectOffsets,
    reference: (u32, u16),
) -> Option<&'a [u8]> {
    let offset = *objects.get(&reference)?;
    let (actual, mut cursor) = pdf_indirect_header_at(document, offset)?;
    if actual != reference {
        return None;
    }
    let dictionary = pdf_dictionary_at(document, &mut cursor)?;
    if !pdf_consume_keyword(document, &mut cursor, b"endobj") {
        return None;
    }
    Some(dictionary)
}

fn pdf_pages_count(
    document: &[u8],
    objects: &PdfObjectOffsets,
    reference: (u32, u16),
    depth: usize,
    visited: &mut std::collections::HashSet<(u32, u16)>,
) -> Option<u32> {
    if depth > MAX_PDF_SYNTAX_DEPTH || !visited.insert(reference) {
        return None;
    }
    let object = pdf_dictionary_object_at(document, objects, reference)?;
    if pdf_object_has_type(object, b"/Page") {
        return Some(1);
    }
    if !pdf_object_has_type(object, b"/Pages") {
        return None;
    }
    let declared = pdf_int_after(object, b"/Count")?;
    if declared == 0 || declared as usize > MAX_CONTAINER_ELEMENTS {
        return None;
    }
    let mut actual = 0u32;
    for child in pdf_kids(object)? {
        actual = actual.checked_add(pdf_pages_count(
            document,
            objects,
            child,
            depth + 1,
            visited,
        )?)?;
    }
    (actual == declared).then_some(actual)
}

fn pdf_reject_unsupported_trailer_features(dictionary: &[u8]) -> Option<()> {
    for key in [b"/Prev".as_slice(), b"/XRefStm", b"/Encrypt"] {
        if pdf_value_after(dictionary, key).ok()?.is_some() {
            return None;
        }
    }
    Some(())
}

fn valid_pdf_xref_table<'a>(
    document: &'a [u8],
    xref_offset: usize,
    startxref_offset: usize,
) -> Option<PdfXref<'a>> {
    let section = document.get(xref_offset..startxref_offset)?;
    if !section.starts_with(b"xref")
        || section.get(4).is_some_and(|byte| !pdf_is_delimiter(*byte))
    {
        return None;
    }
    let trailer_offset = find_bytes(section, b"trailer")?;
    let table = std::str::from_utf8(&section[4..trailer_offset]).ok()?;
    let mut lines = table.lines().map(str::trim).filter(|line| !line.is_empty());
    let mut indexed = std::collections::HashSet::new();
    let mut objects = PdfObjectOffsets::new();
    while let Some(header) = lines.next() {
        let mut fields = header.split_ascii_whitespace();
        let first = fields.next()?.parse::<u32>().ok()?;
        let count = fields.next()?.parse::<usize>().ok()?;
        if fields.next().is_some()
            || count == 0
            || indexed.len().checked_add(count)? > MAX_CONTAINER_ELEMENTS
        {
            return None;
        }
        for index in 0..count {
            let object = first.checked_add(u32::try_from(index).ok()?)?;
            if !indexed.insert(object) {
                return None;
            }
            let mut fields = lines.next()?.split_ascii_whitespace();
            let offset = fields.next()?;
            let generation = fields.next()?;
            let state = fields.next()?;
            if fields.next().is_some()
                || offset.len() != 10
                || generation.len() != 5
                || !offset.bytes().all(|byte| byte.is_ascii_digit())
                || !generation.bytes().all(|byte| byte.is_ascii_digit())
                || !matches!(state, "n" | "f")
            {
                return None;
            }
            if state == "n" {
                let generation = generation.parse::<u16>().ok()?;
                let offset = offset.parse::<usize>().ok()?;
                let (actual, _) = pdf_indirect_header_at(document, offset)?;
                if offset >= xref_offset
                    || actual != (object, generation)
                    || objects.insert(actual, offset).is_some()
                {
                    return None;
                }
            }
        }
    }
    if objects.is_empty() {
        return None;
    }
    let mut trailer_cursor = trailer_offset.checked_add(b"trailer".len())?;
    let trailer = pdf_dictionary_at(section, &mut trailer_cursor)?;
    pdf_skip_space_and_comments(section, &mut trailer_cursor);
    if trailer_cursor != section.len() {
        return None;
    }
    pdf_reject_unsupported_trailer_features(trailer)?;
    let size = pdf_int_after(trailer, b"/Size")?;
    if size == 0
        || size as usize > MAX_CONTAINER_ELEMENTS
        || indexed.iter().any(|object| *object >= size)
    {
        return None;
    }
    Some(PdfXref { objects, trailer })
}

fn pdf_flate_filter(dictionary: &[u8]) -> Option<bool> {
    let filter = pdf_value_after(dictionary, b"/Filter").ok()?;
    let decode_parameters = pdf_value_after(dictionary, b"/DecodeParms").ok()?;
    let Some(filter) = filter else {
        return decode_parameters.is_none().then_some(false);
    };
    let name = if let Some(name) = pdf_value_name(filter) {
        name
    } else {
        let mut offset = 0usize;
        pdf_skip_space_and_comments(filter, &mut offset);
        if filter.get(offset) != Some(&b'[') {
            return None;
        }
        offset += 1;
        pdf_skip_space_and_comments(filter, &mut offset);
        let name = pdf_parse_name(filter, &mut offset, filter.len())?;
        pdf_skip_space_and_comments(filter, &mut offset);
        if filter.get(offset) != Some(&b']') {
            return None;
        }
        offset += 1;
        pdf_skip_space_and_comments(filter, &mut offset);
        if offset != filter.len() {
            return None;
        }
        name
    };
    if !matches!(name, b"/FlateDecode" | b"/Fl") {
        return None;
    }
    if let Some(parameters) = decode_parameters {
        let mut offset = 0usize;
        if !pdf_consume_keyword(parameters, &mut offset, b"null") {
            return None;
        }
        pdf_skip_space_and_comments(parameters, &mut offset);
        if offset != parameters.len() {
            return None;
        }
    }
    Some(true)
}

fn pdf_xref_field(bytes: &[u8], offset: &mut usize, width: usize) -> Option<u64> {
    if width == 0 {
        return Some(0);
    }
    let end = offset.checked_add(width)?;
    let mut value = 0u64;
    for byte in bytes.get(*offset..end)? {
        value = value.checked_shl(8)? | u64::from(*byte);
    }
    *offset = end;
    Some(value)
}

fn valid_pdf_xref_stream<'a>(
    document: &'a [u8],
    xref_offset: usize,
    startxref_offset: usize,
) -> Option<PdfXref<'a>> {
    let (xref_reference, mut cursor) = pdf_indirect_header_at(document, xref_offset)?;
    let trailer = pdf_dictionary_at(document, &mut cursor)?;
    if !pdf_object_has_type(trailer, b"/XRef") {
        return None;
    }
    pdf_reject_unsupported_trailer_features(trailer)?;
    let size = pdf_int_after(trailer, b"/Size")?;
    if size == 0 || size as usize > MAX_CONTAINER_ELEMENTS || xref_reference.0 >= size {
        return None;
    }
    let widths = pdf_value_uint_array(pdf_value_after(trailer, b"/W").ok()??, 3)?;
    if widths.len() != 3 || widths.iter().any(|width| *width > 8) {
        return None;
    }
    let widths = [
        usize::try_from(widths[0]).ok()?,
        usize::try_from(widths[1]).ok()?,
        usize::try_from(widths[2]).ok()?,
    ];
    let entry_width = widths.iter().try_fold(0usize, |sum, width| sum.checked_add(*width))?;
    if entry_width == 0 {
        return None;
    }
    let index = match pdf_value_after(trailer, b"/Index").ok()? {
        Some(value) => pdf_value_uint_array(value, MAX_CONTAINER_ELEMENTS.checked_mul(2)?)?,
        None => vec![0, size],
    };
    if index.is_empty() || index.len() % 2 != 0 {
        return None;
    }
    let mut seen = std::collections::HashSet::new();
    let mut object_numbers = Vec::new();
    for range in index.chunks_exact(2) {
        let first = range[0];
        let count = range[1];
        let end = first.checked_add(count)?;
        if count == 0 || end > size {
            return None;
        }
        for object in first..end {
            if !seen.insert(object) || object_numbers.len() >= MAX_CONTAINER_ELEMENTS {
                return None;
            }
            object_numbers.push(object);
        }
    }
    let expected_bytes = object_numbers.len().checked_mul(entry_width)?;
    if expected_bytes == 0 || expected_bytes > MAX_PDF_XREF_STREAM_BYTES {
        return None;
    }
    let length = usize::try_from(pdf_int_after(trailer, b"/Length")?).ok()?;
    if length == 0 || length > MAX_PDF_XREF_STREAM_BYTES {
        return None;
    }
    if !pdf_consume_keyword(document, &mut cursor, b"stream") {
        return None;
    }
    match document.get(cursor..cursor + 2) {
        Some(b"\r\n") => cursor += 2,
        _ if document.get(cursor) == Some(&b'\n') || document.get(cursor) == Some(&b'\r') => {
            cursor += 1;
        }
        _ => return None,
    }
    let payload_end = cursor.checked_add(length)?;
    let payload = document.get(cursor..payload_end)?;
    cursor = payload_end;
    match document.get(cursor..cursor + 2) {
        Some(b"\r\n") => cursor += 2,
        _ if document.get(cursor) == Some(&b'\n') || document.get(cursor) == Some(&b'\r') => {
            cursor += 1;
        }
        _ => return None,
    }
    if !pdf_consume_keyword(document, &mut cursor, b"endstream")
        || !pdf_consume_keyword(document, &mut cursor, b"endobj")
    {
        return None;
    }
    pdf_skip_space_and_comments(document, &mut cursor);
    if cursor != startxref_offset {
        return None;
    }
    let decoded = if pdf_flate_filter(trailer)? {
        let mut decoder = flate2::bufread::ZlibDecoder::new(payload);
        let mut output = Vec::with_capacity(expected_bytes);
        (&mut decoder)
            .take(u64::try_from(expected_bytes.checked_add(1)?).ok()?)
            .read_to_end(&mut output)
            .ok()?;
        if output.len() != expected_bytes || decoder.total_in() != payload.len() as u64 {
            return None;
        }
        output
    } else {
        if payload.len() != expected_bytes {
            return None;
        }
        payload.to_vec()
    };
    let mut entry_offset = 0usize;
    let mut objects = PdfObjectOffsets::new();
    for object in object_numbers {
        let entry_type = if widths[0] == 0 {
            1
        } else {
            pdf_xref_field(&decoded, &mut entry_offset, widths[0])?
        };
        let field_1 = pdf_xref_field(&decoded, &mut entry_offset, widths[1])?;
        let field_2 = pdf_xref_field(&decoded, &mut entry_offset, widths[2])?;
        match entry_type {
            0 => {}
            1 => {
                let offset = usize::try_from(field_1).ok()?;
                let generation = u16::try_from(field_2).ok()?;
                let reference = (object, generation);
                let (actual, _) = pdf_indirect_header_at(document, offset)?;
                if offset > xref_offset
                    || (offset == xref_offset && reference != xref_reference)
                    || actual != reference
                    || objects.insert(reference, offset).is_some()
                {
                    return None;
                }
            }
            // Type 2 points into an /ObjStm. Object-stream extraction is not
            // implemented here, so accepting it would make reachability a lie.
            2 => return None,
            _ => return None,
        }
    }
    if entry_offset != decoded.len() || objects.get(&xref_reference) != Some(&xref_offset) {
        return None;
    }
    Some(PdfXref { objects, trailer })
}

fn valid_pdf(bytes: &[u8]) -> bool {
    if bytes.len() < 16
        || !bytes.starts_with(b"%PDF-")
        || !bytes.get(5).is_some_and(u8::is_ascii_digit)
        || bytes.get(6) != Some(&b'.')
        || !bytes.get(7).is_some_and(u8::is_ascii_digit)
    {
        return false;
    }
    let Some(eof) = rfind_bytes(bytes, b"%%EOF") else {
        return false;
    };
    if !bytes[eof + 5..].iter().all(u8::is_ascii_whitespace) {
        return false;
    }
    let document = &bytes[..eof];
    let Some(startxref) = rfind_bytes(document, b"startxref") else {
        return false;
    };
    if !contains_bytes(&document[..startxref], b" obj")
        || !contains_bytes(&document[..startxref], b"endobj")
    {
        return false;
    }
    let offset_tail = &document[startxref + b"startxref".len()..];
    let leading_whitespace = offset_tail
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(offset_tail.len());
    let offset_tail = &offset_tail[leading_whitespace..];
    let digit_count = offset_tail
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(offset_tail.len());
    if digit_count == 0
        || digit_count > 20
        || !offset_tail[digit_count..].iter().all(u8::is_ascii_whitespace)
    {
        return false;
    }
    let Ok(offset_text) = std::str::from_utf8(&offset_tail[..digit_count]) else {
        return false;
    };
    let Ok(xref_offset) = offset_text.parse::<usize>() else {
        return false;
    };
    let Some(xref) = (if document.get(xref_offset..).is_some_and(|tail| tail.starts_with(b"xref")) {
        valid_pdf_xref_table(document, xref_offset, startxref)
    } else {
        valid_pdf_xref_stream(document, xref_offset, startxref)
    }) else {
        return false;
    };
    let Some(root) = pdf_ref_after(xref.trailer, b"/Root") else {
        return false;
    };
    if !xref.objects.contains_key(&root) {
        return false;
    }
    let Some(catalog) = pdf_dictionary_object_at(document, &xref.objects, root) else {
        return false;
    };
    if !pdf_object_has_type(catalog, b"/Catalog") {
        return false;
    }
    let Some(pages) = pdf_ref_after(catalog, b"/Pages") else {
        return false;
    };
    pdf_pages_count(
        document,
        &xref.objects,
        pages,
        0,
        &mut std::collections::HashSet::new(),
    )
    .is_some_and(|count| count > 0)
}

#[derive(Debug, Clone, Copy)]
enum ZipFlavor {
    Generic,
    Docx,
    Xlsx,
    Pptx,
}

fn valid_zip(bytes: &[u8], flavor: ZipFlavor) -> bool {
    use std::collections::HashSet;

    let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return false;
    };
    if archive.len() == 0 || archive.len() > MAX_ZIP_ENTRIES {
        return false;
    }
    let mut names = HashSet::with_capacity(archive.len());
    let mut expanded = 0_u64;
    let mut regular_files = 0usize;
    let mut content_types = None;
    let mut package_relationships = None;
    let mut main_part_bytes = None;
    let (required_main_part, required_content_type, required_root_markers): (
        Option<&str>,
        Option<&[u8]>,
        &[&[u8]],
    ) = match flavor {
        ZipFlavor::Generic => (None, None, &[]),
        ZipFlavor::Docx => (
            Some("word/document.xml"),
            Some(b"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"),
            &[b"<w:document", b"<document"],
        ),
        ZipFlavor::Xlsx => (
            Some("xl/workbook.xml"),
            Some(b"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"),
            &[b"<workbook", b":workbook"],
        ),
        ZipFlavor::Pptx => (
            Some("ppt/presentation.xml"),
            Some(b"application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"),
            &[b"<p:presentation", b"<presentation"],
        ),
    };

    for index in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(index) else {
            return false;
        };
        let name = entry.name().to_owned();
        if name.is_empty()
            || name.contains('\\')
            || name.starts_with('/')
            || name.split('/').any(|part| matches!(part, "." | ".."))
            || !names.insert(name.clone())
        {
            return false;
        }
        let Some(next_expanded) = expanded.checked_add(entry.size()) else {
            return false;
        };
        if next_expanded > MAX_ZIP_EXPANDED_BYTES {
            return false;
        }
        expanded = next_expanded;

        if entry.is_dir() {
            if entry.size() != 0 {
                return false;
            }
            continue;
        }
        regular_files += 1;
        let should_capture = name == "[Content_Types].xml"
            || name == "_rels/.rels"
            || required_main_part == Some(name.as_str());
        if should_capture {
            if entry.size() > 64 * 1024 * 1024 {
                return false;
            }
            let expected = entry.size();
            let Ok(capacity) = usize::try_from(expected) else {
                return false;
            };
            let mut value = Vec::with_capacity(capacity);
            let Some(limit) = expected.checked_add(1) else {
                return false;
            };
            let Ok(copied) = (&mut entry).take(limit).read_to_end(&mut value) else {
                return false;
            };
            if copied as u64 != expected || value.len() as u64 != expected {
                return false;
            }
            if name == "[Content_Types].xml" {
                content_types = Some(value);
            } else if name == "_rels/.rels" {
                package_relationships = Some(value);
            } else {
                main_part_bytes = Some(value);
            }
        } else {
            let expected = entry.size();
            let Some(limit) = expected.checked_add(1) else {
                return false;
            };
            let Ok(copied) = std::io::copy(&mut (&mut entry).take(limit), &mut std::io::sink()) else {
                return false;
            };
            if copied != expected {
                return false;
            }
        }
    }
    if regular_files == 0 {
        return false;
    }

    if matches!(flavor, ZipFlavor::Generic) {
        return true;
    }
    let (Some(main_part), Some(main_content_type)) = (required_main_part, required_content_type) else {
        return false;
    };
    let xml_is_usable = |value: &[u8], markers: &[&[u8]]| {
        !value.is_empty()
            && std::str::from_utf8(value).is_ok()
            && markers.iter().any(|marker| contains_bytes(value, marker))
    };
    names.contains("_rels/.rels")
        && names.contains(main_part)
        && content_types.as_deref().is_some_and(|value| {
            xml_is_usable(value, &[b"<Types", b":Types"])
                && contains_bytes(value, main_content_type)
        })
        && package_relationships
            .as_deref()
            .is_some_and(|value| xml_is_usable(value, &[b"<Relationships", b":Relationships"]))
        && main_part_bytes
            .as_deref()
            .is_some_and(|value| xml_is_usable(value, required_root_markers))
}

fn sniff_known_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
    {
        Some("application/zip")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") {
        Some("audio/wav")
    } else if bytes.starts_with(b"OggS") {
        Some("audio/ogg")
    } else if bytes.starts_with(b"fLaC") {
        Some("audio/flac")
    } else if bytes.starts_with(b"ID3")
        || (bytes.len() >= 2 && bytes[0] == 0xff && (bytes[1] & 0xe0) == 0xe0)
    {
        Some("audio/mpeg")
    } else if let Some(info) = iso_bmff_info(bytes) {
        if info.has_video {
            Some("video/mp4")
        } else {
            Some("audio/mp4")
        }
    } else if let Some(info) = webm_info(bytes) {
        if info.has_video {
            Some("video/webm")
        } else {
            Some("audio/webm")
        }
    } else {
        None
    }
}

fn extension_for_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "text/plain" => Some("txt"),
        "text/markdown" => Some("md"),
        "text/html" => Some("html"),
        "text/csv" => Some("csv"),
        "application/json" => Some("json"),
        "application/xml" => Some("xml"),
        "application/pdf" => Some("pdf"),
        "application/zip" => Some("zip"),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => Some("docx"),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => Some("xlsx"),
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => Some("pptx"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn bmff_box_bytes(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(payload.len() + 8);
        output.extend_from_slice(&u32::try_from(payload.len() + 8).unwrap().to_be_bytes());
        output.extend_from_slice(kind);
        output.extend_from_slice(payload);
        output
    }

    fn bmff(brand: &[u8; 4]) -> Vec<u8> {
        let handler = if brand == b"M4A " { b"soun" } else { b"vide" };
        let sample_entry = bmff_box_bytes(if handler == b"soun" { b"mp4a" } else { b"avc1" }, &[1; 8]);
        let mut stsd = vec![0; 4];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&sample_entry);
        let mut stts = vec![0; 4];
        stts.extend_from_slice(&1u32.to_be_bytes());
        stts.extend_from_slice(&1u32.to_be_bytes());
        stts.extend_from_slice(&1u32.to_be_bytes());
        let mut stsc = vec![0; 4];
        stsc.extend_from_slice(&1u32.to_be_bytes());
        stsc.extend_from_slice(&1u32.to_be_bytes());
        stsc.extend_from_slice(&1u32.to_be_bytes());
        stsc.extend_from_slice(&1u32.to_be_bytes());
        let mut stsz = vec![0; 4];
        stsz.extend_from_slice(&4u32.to_be_bytes());
        stsz.extend_from_slice(&1u32.to_be_bytes());
        let mut stco = vec![0; 4];
        stco.extend_from_slice(&1u32.to_be_bytes());
        stco.extend_from_slice(&1u32.to_be_bytes());
        let stbl = [
            bmff_box_bytes(b"stsd", &stsd),
            bmff_box_bytes(b"stts", &stts),
            bmff_box_bytes(b"stsc", &stsc),
            bmff_box_bytes(b"stsz", &stsz),
            bmff_box_bytes(b"stco", &stco),
        ]
        .concat();
        let minf = bmff_box_bytes(b"stbl", &stbl);
        let mut hdlr = vec![0; 12];
        hdlr[8..12].copy_from_slice(handler);
        let mdia = [
            bmff_box_bytes(b"mdhd", &[1; 8]),
            bmff_box_bytes(b"hdlr", &hdlr),
            bmff_box_bytes(b"minf", &minf),
        ]
        .concat();
        let trak = [
            bmff_box_bytes(b"tkhd", &[1; 8]),
            bmff_box_bytes(b"mdia", &mdia),
        ]
        .concat();
        let moov = [
            bmff_box_bytes(b"mvhd", &[1; 8]),
            bmff_box_bytes(b"trak", &trak),
        ]
        .concat();
        let mut ftyp = brand.to_vec();
        ftyp.extend_from_slice(&[0; 4]);
        [
            bmff_box_bytes(b"ftyp", &ftyp),
            bmff_box_bytes(b"moov", &moov),
            bmff_box_bytes(b"mdat", &[1, 2, 3, 4]),
        ]
        .concat()
    }

    fn wav() -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&38u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 0, 1, 0]);
        bytes.extend_from_slice(&8_000u32.to_le_bytes());
        bytes.extend_from_slice(&8_000u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 0, 8, 0]);
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&[128, 0]);
        bytes
    }

    fn ogg() -> Vec<u8> {
        let mut opus = b"OpusHead".to_vec();
        opus.extend_from_slice(&[1, 1, 0, 0]);
        opus.extend_from_slice(&48_000u32.to_le_bytes());
        opus.extend_from_slice(&[0, 0, 0]);
        assert_eq!(opus.len(), 19);
        let audio = [0xf8, 1, 2, 3, 4];
        let mut bytes = vec![0; 27];
        bytes[..4].copy_from_slice(b"OggS");
        bytes[5] = 0x06;
        bytes[6..14].copy_from_slice(&960u64.to_le_bytes());
        bytes[14..18].copy_from_slice(&1u32.to_le_bytes());
        bytes[26] = 2;
        bytes.extend_from_slice(&[opus.len() as u8, audio.len() as u8]);
        bytes.extend_from_slice(&opus);
        bytes.extend_from_slice(&audio);
        let crc = ogg_crc(&bytes);
        bytes[22..26].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    fn flac() -> Vec<u8> {
        let mut bytes = b"fLaC".to_vec();
        bytes.extend_from_slice(&[0x80, 0, 0, 34]);
        let mut stream_info = [0_u8; 34];
        stream_info[..2].copy_from_slice(&16_u16.to_be_bytes());
        stream_info[2..4].copy_from_slice(&16_u16.to_be_bytes());
        let stream_word = (8_000_u64 << 44) | (7_u64 << 36) | 16;
        stream_info[10..18].copy_from_slice(&stream_word.to_be_bytes());
        bytes.extend_from_slice(&stream_info);
        let mut frame = vec![0xff, 0xf8, 0x60, 0x02, 0, 15];
        frame.push(flac_crc8(&frame));
        frame.extend_from_slice(&[0, 1]); // constant mono subframe, 8-bit sample
        let crc = frame
            .iter()
            .fold(0_u16, |crc, byte| flac_crc16_update(crc, *byte));
        bytes.extend_from_slice(&frame);
        bytes.extend_from_slice(&crc.to_be_bytes());
        bytes
    }

    fn mp3() -> Vec<u8> {
        // Two complete MPEG-1 Layer III, 128 kbps, 44.1 kHz frames.
        let mut frame = vec![0; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
        frame[10] = 1;
        [frame.clone(), frame].concat()
    }

    fn ebml_element(id: &[u8], payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() < 127);
        let mut output = id.to_vec();
        output.push(0x80 | payload.len() as u8);
        output.extend_from_slice(payload);
        output
    }

    fn webm() -> Vec<u8> {
        let header = ebml_element(&[0x42, 0x82], b"webm");
        let info = ebml_element(&[0x2a, 0xd7, 0xb1], &[0x0f, 0x42, 0x40]);
        let track = [
            ebml_element(&[0xd7], &[1]),
            ebml_element(&[0x83], &[1]),
            ebml_element(&[0x86], b"V_VP8"),
        ]
        .concat();
        let tracks = ebml_element(&[0xae], &track);
        let cluster = [
            ebml_element(&[0xe7], &[0]),
            ebml_element(&[0xa3], &[0x81, 0, 0, 0, 1]),
        ]
        .concat();
        let segment = [
            ebml_element(&[0x15, 0x49, 0xa9, 0x66], &info),
            ebml_element(&[0x16, 0x54, 0xae, 0x6b], &tracks),
            ebml_element(&[0x1f, 0x43, 0xb6, 0x75], &cluster),
        ]
        .concat();
        [
            ebml_element(&[0x1a, 0x45, 0xdf, 0xa3], &header),
            ebml_element(&[0x18, 0x53, 0x80, 0x67], &segment),
        ]
        .concat()
    }

    fn pdf() -> Vec<u8> {
        let mut bytes = b"%PDF-1.4\n".to_vec();
        let mut offsets = vec![0usize];
        for object in [
            b"<< /Type /Catalog /Pages 2 0 R >>".as_slice(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".as_slice(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>".as_slice(),
        ] {
            offsets.push(bytes.len());
            let number = offsets.len() - 1;
            bytes.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
            bytes.extend_from_slice(object);
            bytes.extend_from_slice(b"\nendobj\n");
        }
        let xref_offset = bytes.len();
        bytes.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
        for offset in offsets.iter().skip(1) {
            bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        bytes.extend_from_slice(b"trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n");
        bytes.extend_from_slice(xref_offset.to_string().as_bytes());
        bytes.extend_from_slice(b"\n%%EOF\n");
        bytes
    }

    fn pdf_xref_entry(kind: u8, field_1: u32, field_2: u16) -> [u8; 7] {
        let mut entry = [0u8; 7];
        entry[0] = kind;
        entry[1..5].copy_from_slice(&field_1.to_be_bytes());
        entry[5..7].copy_from_slice(&field_2.to_be_bytes());
        entry
    }

    fn pdf_xref_stream(
        index: Option<&str>,
        widths: [u32; 3],
        mutate_entries: impl FnOnce(&mut Vec<[u8; 7]>),
    ) -> Vec<u8> {
        let mut bytes = b"%PDF-1.5\n".to_vec();
        let mut offsets = vec![0usize];
        for object in [
            b"<< /Type /Catalog /Pages 2 0 R >>".as_slice(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".as_slice(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>".as_slice(),
        ] {
            offsets.push(bytes.len());
            let number = offsets.len() - 1;
            bytes.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
            bytes.extend_from_slice(object);
            bytes.extend_from_slice(b"\nendobj\n");
        }

        let xref_offset = bytes.len();
        let mut entries = vec![pdf_xref_entry(0, 0, u16::MAX)];
        entries.extend(
            offsets
                .iter()
                .skip(1)
                .map(|offset| pdf_xref_entry(1, u32::try_from(*offset).unwrap(), 0)),
        );
        entries.push(pdf_xref_entry(
            1,
            u32::try_from(xref_offset).unwrap(),
            0,
        ));
        mutate_entries(&mut entries);
        let raw = entries.into_iter().flatten().collect::<Vec<_>>();
        let mut encoder = flate2::write::ZlibEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        );
        encoder.write_all(&raw).unwrap();
        let compressed = encoder.finish().unwrap();
        let index = index.map(|value| format!(" /Index {value}")).unwrap_or_default();
        bytes.extend_from_slice(b"4 0 obj\n");
        bytes.extend_from_slice(
            format!(
                "<< /Type /XRef /Size 5 /W [{} {} {}]{} /Root 1 0 R /Length {} /Filter /FlateDecode >>\nstream\n",
                widths[0], widths[1], widths[2], index, compressed.len()
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(&compressed);
        bytes.extend_from_slice(b"\nendstream\nendobj\nstartxref\n");
        bytes.extend_from_slice(xref_offset.to_string().as_bytes());
        bytes.extend_from_slice(b"\n%%EOF\n");
        bytes
    }

    fn zip_with(files: &[(&str, &[u8])]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in files {
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn docx() -> Vec<u8> {
        zip_with(&[
            (
                "[Content_Types].xml",
                br#"<Types><Override ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#,
            ),
            ("_rels/.rels", b"<Relationships/>") ,
            ("word/document.xml", b"<w:document/>") ,
        ])
    }

    fn xlsx() -> Vec<u8> {
        zip_with(&[
            (
                "[Content_Types].xml",
                br#"<Types><Override ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#,
            ),
            ("_rels/.rels", b"<Relationships/>"),
            ("xl/workbook.xml", b"<workbook/>"),
        ])
    }

    fn pptx() -> Vec<u8> {
        zip_with(&[
            (
                "[Content_Types].xml",
                br#"<Types><Override ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/></Types>"#,
            ),
            ("_rels/.rels", b"<Relationships/>"),
            ("ppt/presentation.xml", b"<p:presentation/>"),
        ])
    }

    #[test]
    fn persists_and_reads_back_valid_image() {
        let temp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(temp.path());
        let artifact = store
            .persist_inline(ArtifactKind::Image, "image/png", ONE_PIXEL_PNG)
            .unwrap();

        assert_eq!(artifact.kind, ArtifactKind::Image);
        assert_eq!(artifact.mime_type, "image/png");
        assert!(artifact.relative_path.starts_with("nomifun-artifacts/"));
        assert!(Path::new(&artifact.path).is_file());
        assert!(fs::read(&artifact.path).unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn verifies_existing_workspace_file_and_rejects_empty_or_outside_paths() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("report.md"), b"# Generated report\n").unwrap();
        fs::write(workspace.path().join("empty.txt"), b"").unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("outside.txt");
        fs::write(&outside, b"outside").unwrap();
        let store = ArtifactStore::new(workspace.path());

        let artifact = store.verify_existing_path("report.md").unwrap();
        assert_eq!(artifact.kind, ArtifactKind::Text);
        assert_eq!(artifact.mime_type, "text/markdown");
        assert_eq!(artifact.relative_path, "report.md");
        assert_eq!(artifact.size_bytes, 19);
        assert_eq!(artifact.sha256.len(), 64);

        assert!(matches!(
            store.verify_existing_path("empty.txt"),
            Err(ArtifactStoreError::VerificationFailed)
        ));
        assert!(matches!(
            store.verify_existing_path(&outside),
            Err(ArtifactStoreError::OutsideWorkspace)
        ));
    }

    #[test]
    fn imported_workspace_file_is_an_immutable_snapshot_and_batch_is_atomic() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("report.md");
        fs::write(&source, b"original report").unwrap();
        let store = ArtifactStore::new(workspace.path());

        let artifact = store.import_existing_path(&source).unwrap();
        assert!(artifact.relative_path.starts_with("nomifun-artifacts/"));
        assert_ne!(Path::new(&artifact.path), source.as_path());
        assert_eq!(fs::read(&artifact.path).unwrap(), b"original report");
        fs::write(&source, b"replaced report").unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"replaced report");
        assert_eq!(fs::read(&artifact.path).unwrap(), b"original report");

        let second_workspace = tempfile::tempdir().unwrap();
        fs::write(second_workspace.path().join("valid.md"), b"valid").unwrap();
        fs::write(second_workspace.path().join("invalid.pdf"), b"not a pdf").unwrap();
        let second_store = ArtifactStore::new(second_workspace.path());
        assert!(second_store
            .import_existing_batch(["valid.md", "invalid.pdf"])
            .is_err());
        assert!(!second_store.artifact_root().exists());
    }

    #[test]
    fn existing_media_path_requires_bytes_matching_its_extension() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("fake.png"), b"not an image").unwrap();
        let store = ArtifactStore::new(workspace.path());

        assert!(matches!(
            store.verify_existing_path("fake.png"),
            Err(ArtifactStoreError::InvalidImage(_))
        ));
    }

    #[test]
    fn generic_batch_persists_audio_and_text_all_or_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(temp.path());
        let audio = b64(&mp3());
        let artifacts = store
            .persist_inline_batch([
                (ArtifactKind::Audio, "audio/mpeg", audio.as_str()),
                (
                    ArtifactKind::Text,
                    "application/json",
                    "eyJ1cmkiOiJodHRwczovL2UifQ==",
                ),
            ])
            .unwrap();

        assert_eq!(artifacts.len(), 2);
        assert_eq!(artifacts[0].kind, ArtifactKind::Audio);
        assert_eq!(artifacts[1].kind, ArtifactKind::Text);
        assert!(artifacts.iter().all(|artifact| Path::new(&artifact.path).is_file()));
    }

    #[test]
    fn generic_batch_validates_every_item_before_writing() {
        let temp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(temp.path());
        let audio = b64(&mp3());
        let result = store.persist_inline_batch([
            (ArtifactKind::Audio, "audio/mpeg", audio.as_str()),
            (ArtifactKind::File, "application/pdf", "bm90IGEgcGRm"),
        ]);

        assert!(result.is_err());
        assert!(!temp.path().join(ARTIFACT_DIRECTORY).exists());
    }

    #[test]
    fn rejects_empty_invalid_and_mismatched_images_without_files() {
        let temp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(temp.path());

        assert!(matches!(
            store.persist_inline(ArtifactKind::Image, "image/png", ""),
            Err(ArtifactStoreError::Empty)
        ));
        assert!(matches!(
            store.persist_inline(ArtifactKind::Image, "image/png", "not base64***"),
            Err(ArtifactStoreError::InvalidBase64)
        ));
        assert!(matches!(
            store.persist_inline(ArtifactKind::Image, "image/jpeg", ONE_PIXEL_PNG),
            Err(ArtifactStoreError::MimeMismatch { .. })
        ));
        assert!(!store.artifact_root().exists());
    }

    #[test]
    fn batch_is_all_or_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(temp.path());
        let result = store.persist_images([
            ("image/png", ONE_PIXEL_PNG),
            ("image/png", "aW52YWxpZCBpbWFnZQ=="),
        ]);

        assert!(result.is_err());
        assert!(!store.artifact_root().exists());
    }

    #[test]
    fn validates_text_and_common_file_receipts() {
        let temp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(temp.path());

        let text = store.persist_text(Some("text/markdown"), "# Generated").unwrap();
        assert_eq!(text.kind, ArtifactKind::Text);
        assert!(text.relative_path.ends_with(".md"));
        assert!(store.persist_text(Some("application/pdf"), "not a pdf").is_err());
        assert!(store.persist_text(Some("application/json"), "not json").is_err());

        let pdf = b64(&pdf());
        let artifact = store
            .persist_inline(ArtifactKind::File, "application/pdf", &pdf)
            .unwrap();
        assert_eq!(artifact.kind, ArtifactKind::File);
        assert!(artifact.relative_path.ends_with(".pdf"));

        let docx = b64(&docx());
        let artifact = store
            .persist_inline(
                ArtifactKind::File,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                &docx,
            )
            .unwrap();
        assert!(artifact.relative_path.ends_with(".docx"));
    }

    #[test]
    fn validates_complete_audio_and_video_containers() {
        let temp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(temp.path());
        let cases = [
            (ArtifactKind::Audio, "audio/wav", wav()),
            (ArtifactKind::Audio, "audio/ogg", ogg()),
            (ArtifactKind::Audio, "audio/flac", flac()),
            (ArtifactKind::Audio, "audio/mpeg", mp3()),
            (ArtifactKind::Audio, "audio/mp4", bmff(b"M4A ")),
            (ArtifactKind::Video, "video/mp4", bmff(b"isom")),
            (ArtifactKind::Video, "video/quicktime", bmff(b"qt  ")),
            (ArtifactKind::Video, "video/webm", webm()),
        ];

        for (kind, mime, bytes) in cases {
            let artifact = store.persist_inline(kind, mime, &b64(&bytes)).unwrap();
            assert_eq!(artifact.kind, kind);
            assert_eq!(artifact.mime_type, mime);
            assert!(Path::new(&artifact.path).is_file());
        }
    }

    #[test]
    fn rejects_header_only_and_truncated_media_inline_and_existing() {
        let temp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(temp.path());
        let cases = [
            (ArtifactKind::Audio, "audio/wav", "wav", wav(), b"RIFF\0\0\0\0WAVE".to_vec()),
            (ArtifactKind::Audio, "audio/ogg", "ogg", ogg(), b"OggS".to_vec()),
            (ArtifactKind::Audio, "audio/flac", "flac", flac(), b"fLaC".to_vec()),
            (ArtifactKind::Audio, "audio/mpeg", "mp3", mp3(), b"ID3".to_vec()),
            (
                ArtifactKind::Video,
                "video/mp4",
                "mp4",
                bmff(b"isom"),
                b"\0\0\0\x10ftypisom".to_vec(),
            ),
            (
                ArtifactKind::Video,
                "video/webm",
                "webm",
                webm(),
                vec![0x1a, 0x45, 0xdf, 0xa3],
            ),
        ];

        for (index, (kind, mime, extension, complete, header)) in cases.into_iter().enumerate() {
            assert!(store.persist_inline(kind, mime, &b64(&header)).is_err());
            assert!(store
                .persist_inline(kind, mime, &b64(&complete[..complete.len() - 1]))
                .is_err());

            let header_path = temp.path().join(format!("header-{index}.{extension}"));
            fs::write(&header_path, header).unwrap();
            assert!(store.verify_existing_path(&header_path).is_err());
            let truncated_path = temp.path().join(format!("truncated-{index}.{extension}"));
            fs::write(&truncated_path, &complete[..complete.len() - 1]).unwrap();
            assert!(store.verify_existing_path(&truncated_path).is_err());
        }
    }

    #[test]
    fn rejects_structural_shells_even_when_outer_lengths_and_checksums_match() {
        let mut bmff_shell = bmff_box_bytes(b"ftyp", b"isom\0\0\0\0");
        bmff_shell.extend_from_slice(&bmff_box_bytes(b"moov", &[0]));
        bmff_shell.extend_from_slice(&bmff_box_bytes(b"mdat", &[1]));
        assert!(iso_bmff_info(&bmff_shell).is_none());

        let webm_shell = [
            ebml_element(&[0x1a, 0x45, 0xdf, 0xa3], &ebml_element(&[0x42, 0x82], b"webm")),
            ebml_element(&[0x18, 0x53, 0x80, 0x67], &[0]),
        ]
        .concat();
        assert!(webm_info(&webm_shell).is_none());

        let mut bad_ogg = ogg();
        let codec = bad_ogg.windows(b"OpusHead".len()).position(|window| window == b"OpusHead").unwrap();
        bad_ogg[codec] = b'X';
        bad_ogg[22..26].fill(0);
        let crc = ogg_crc(&bad_ogg);
        bad_ogg[22..26].copy_from_slice(&crc.to_le_bytes());
        assert!(!valid_ogg(&bad_ogg));

        let mut zero_mp3 = mp3();
        for frame in zero_mp3.chunks_mut(417) {
            frame[4..].fill(0);
        }
        assert!(!valid_mp3(&zero_mp3));

        let mut bad_flac = flac();
        *bad_flac.last_mut().unwrap() ^= 1;
        assert!(!valid_flac(&bad_flac));
    }

    #[test]
    fn rejects_incomplete_pdf_and_zip_but_accepts_complete_structures() {
        let temp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(temp.path());
        let pdf = pdf();
        let archive = zip_with(&[("report.txt", b"complete")]);
        let mut corrupt_archive = archive.clone();
        let payload = corrupt_archive
            .windows(b"complete".len())
            .position(|window| window == b"complete")
            .unwrap();
        corrupt_archive[payload] ^= 0xff;

        assert!(store
            .persist_inline(ArtifactKind::File, "application/pdf", &b64(b"%PDF-1.7\n%%EOF"))
            .is_err());
        // Spotting `/Type /XRef` without a decodable, cross-checked index is not
        // proof of a usable Catalog/Pages tree.
        assert!(store
            .persist_inline(
                ArtifactKind::File,
                "application/pdf",
                &b64(b"%PDF-1.5\n1 0 obj\n<< /Type /XRef /Length 1 >>\nstream\n0\nendstream\nendobj\nstartxref\n9\n%%EOF\n"),
            )
            .is_err());
        assert!(store
            .persist_inline(ArtifactKind::File, "application/pdf", &b64(&pdf[..pdf.len() - 2]))
            .is_err());
        assert!(store
            .persist_inline(ArtifactKind::File, "application/zip", &b64(b"PK\x03\x04"))
            .is_err());
        assert!(store
            .persist_inline(
                ArtifactKind::File,
                "application/zip",
                &b64(&archive[..archive.len() - 1]),
            )
            .is_err());
        assert!(store
            .persist_inline(
                ArtifactKind::File,
                "application/zip",
                &b64(&corrupt_archive),
            )
            .is_err());

        assert!(store
            .persist_inline(ArtifactKind::File, "application/pdf", &b64(&pdf))
            .is_ok());
        assert!(store
            .persist_inline(ArtifactKind::File, "application/zip", &b64(&archive))
            .is_ok());

        fs::write(temp.path().join("truncated.pdf"), &pdf[..pdf.len() - 2]).unwrap();
        fs::write(temp.path().join("truncated.zip"), &archive[..archive.len() - 1]).unwrap();
        assert!(store.verify_existing_path("truncated.pdf").is_err());
        assert!(store.verify_existing_path("truncated.zip").is_err());
    }

    #[test]
    fn accepts_complete_flate_pdf_xref_streams_with_default_or_explicit_index() {
        let temp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(temp.path());
        for (index, pdf) in [
            ("default", pdf_xref_stream(None, [1, 4, 2], |_| {})),
            (
                "explicit",
                pdf_xref_stream(Some("[0 2 2 3]"), [1, 4, 2], |_| {}),
            ),
        ] {
            assert!(valid_pdf(&pdf), "{index} xref stream must be valid");
            assert!(store
                .persist_inline(ArtifactKind::File, "application/pdf", &b64(&pdf))
                .is_ok());
            fs::write(temp.path().join(format!("{index}.pdf")), &pdf).unwrap();
            assert!(store
                .verify_existing_path(format!("{index}.pdf"))
                .is_ok());
        }
    }

    #[test]
    fn rejects_malicious_or_unresolvable_pdf_xref_streams() {
        let compressed_object = pdf_xref_stream(None, [1, 4, 2], |entries| {
            entries[1][0] = 2;
        });
        let bad_offset = pdf_xref_stream(None, [1, 4, 2], |entries| {
            entries[1][1..5].copy_from_slice(&u32::MAX.to_be_bytes());
        });
        let duplicate_index = pdf_xref_stream(Some("[0 3 2 3]"), [1, 4, 2], |_| {});
        let oversized_width = pdf_xref_stream(None, [1, 9, 2], |_| {});
        let decompression_overrun = pdf_xref_stream(None, [1, 4, 2], |entries| {
            entries.push(pdf_xref_entry(0, 0, 0));
        });
        let mut corrupt_flate = pdf_xref_stream(None, [1, 4, 2], |_| {});
        let stream = find_bytes(&corrupt_flate, b"stream\n").unwrap() + b"stream\n".len();
        let endstream = find_bytes(&corrupt_flate[stream..], b"\nendstream").unwrap() + stream;
        corrupt_flate[endstream - 1] ^= 0xff;
        let mut false_length = pdf_xref_stream(None, [1, 4, 2], |_| {});
        let length = find_bytes(&false_length, b"/Length ").unwrap() + b"/Length ".len();
        false_length[length] = if false_length[length] == b'9' {
            b'8'
        } else {
            b'9'
        };
        let mut unreachable_pages = pdf_xref_stream(None, [1, 4, 2], |_| {});
        let pages = find_bytes(&unreachable_pages, b"/Pages 2 0 R").unwrap() + b"/Pages ".len();
        unreachable_pages[pages] = b'9';

        for pdf in [
            compressed_object,
            bad_offset,
            duplicate_index,
            oversized_width,
            decompression_overrun,
            corrupt_flate,
            false_length,
            unreachable_pages,
        ] {
            assert!(!valid_pdf(&pdf));
        }
    }

    #[test]
    fn openxml_requires_its_complete_package_parts() {
        let temp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(temp.path());
        let generic_zip = zip_with(&[("word/document.xml", b"<w:document/>")]);
        let mime = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

        assert!(store
            .persist_inline(ArtifactKind::File, mime, &b64(&generic_zip))
            .is_err());
        assert!(store
            .persist_inline(ArtifactKind::File, mime, &b64(&docx()))
            .is_ok());
        assert!(store
            .persist_inline(
                ArtifactKind::File,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                &b64(&xlsx()),
            )
            .is_ok());
        assert!(store
            .persist_inline(
                ArtifactKind::File,
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                &b64(&pptx()),
            )
            .is_ok());
        let empty_main = zip_with(&[
            (
                "[Content_Types].xml",
                br#"<Types><Override ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#,
            ),
            ("_rels/.rels", b"<Relationships/>"),
            ("word/document.xml", b""),
        ]);
        assert!(store
            .persist_inline(ArtifactKind::File, mime, &b64(&empty_main))
            .is_err());
    }

    #[test]
    fn malformed_media_corpus_never_panics() {
        let fixtures = [bmff(b"isom"), webm(), wav(), ogg(), flac(), mp3(), pdf()];
        for fixture in &fixtures {
            for cut in 0..fixture.len() {
                let prefix = &fixture[..cut];
                let result = std::panic::catch_unwind(|| {
                    let _ = iso_bmff_info(prefix);
                    let _ = webm_info(prefix);
                    let _ = valid_wav(prefix);
                    let _ = valid_ogg(prefix);
                    let _ = valid_flac(prefix);
                    let _ = valid_mp3(prefix);
                    let _ = valid_pdf(prefix);
                });
                assert!(result.is_ok(), "validator panicked for prefix length {cut}");
            }
        }

        let mut state = 0x7f4a_7c15_9e37_79b9u64;
        for length in 0..256usize {
            let mut bytes = vec![0u8; length];
            for byte in &mut bytes {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state as u8;
            }
            let result = std::panic::catch_unwind(|| {
                let _ = iso_bmff_info(&bytes);
                let _ = webm_info(&bytes);
                let _ = valid_wav(&bytes);
                let _ = valid_ogg(&bytes);
                let _ = valid_flac(&bytes);
                let _ = valid_mp3(&bytes);
                let _ = valid_pdf(&bytes);
            });
            assert!(result.is_ok(), "validator panicked for corpus length {length}");
        }
    }

    #[test]
    fn generic_binary_is_allowed_but_cannot_hide_a_known_format() {
        let temp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(temp.path());

        let opaque = store
            .persist_inline(
                ArtifactKind::File,
                "application/octet-stream",
                &b64(b"opaque provider-specific bytes"),
            )
            .unwrap();
        assert_eq!(opaque.mime_type, "application/octet-stream");
        assert!(opaque.relative_path.ends_with(".bin"));

        let classified = store
            .persist_inline(ArtifactKind::File, "application/octet-stream", &b64(&pdf()))
            .unwrap();
        assert_eq!(classified.mime_type, "application/pdf");
        assert!(classified.relative_path.ends_with(".pdf"));
        assert!(store
            .persist_inline(
                ArtifactKind::File,
                "application/octet-stream",
                &b64(b"%PDF-1.7\n%%EOF"),
            )
            .is_err());
        assert!(store
            .persist_inline(
                ArtifactKind::File,
                "application/octet-stream",
                &b64(b"generation failed: upstream returned no output"),
            )
            .is_err());
    }

    #[test]
    fn atomic_commit_never_replaces_an_existing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.tmp");
        let target = temp.path().join("target.bin");
        fs::write(&source, b"new bytes").unwrap();
        fs::write(&target, b"original bytes").unwrap();

        let error = durable_rename_no_replace(&source, &target).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&target).unwrap(), b"original bytes");
        assert_eq!(fs::read(&source).unwrap(), b"new bytes");
    }

    #[cfg(unix)]
    #[test]
    fn reservation_fallback_publishes_atomically_and_never_replaces() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.tmp");
        let target = temp.path().join("target.bin");
        fs::write(&source, b"complete generated bytes").unwrap();

        reservation_rename_no_replace(&source, &target).unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(&target).unwrap(), b"complete generated bytes");

        let second_source = temp.path().join("second.tmp");
        fs::write(&second_source, b"must not replace").unwrap();
        let error = reservation_rename_no_replace(&second_source, &target).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&target).unwrap(), b"complete generated bytes");
        assert_eq!(fs::read(&second_source).unwrap(), b"must not replace");
    }

    #[test]
    fn failed_collision_cleanup_never_deletes_an_unowned_target() {
        let temp = tempfile::tempdir().unwrap();
        let private_temp = temp.path().join("private.tmp");
        let existing_target = temp.path().join("existing.bin");
        fs::write(&private_temp, b"private bytes").unwrap();
        fs::write(&existing_target, b"pre-existing bytes").unwrap();

        cleanup_failed_publication(&private_temp, &existing_target, false);

        assert!(!private_temp.exists());
        assert_eq!(fs::read(existing_target).unwrap(), b"pre-existing bytes");
    }

    #[test]
    fn stale_owned_temp_files_are_cleaned_without_touching_other_files() {
        let temp = tempfile::tempdir().unwrap();
        let owned_temp = temp.path().join(".artifact-018f.tmp");
        let unrelated = temp.path().join(".another.tmp");
        fs::write(&owned_temp, b"partial").unwrap();
        fs::write(&unrelated, b"keep").unwrap();

        let future_cutoff = SystemTime::now() + Duration::from_secs(1);
        cleanup_temp_files_before(temp.path(), future_cutoff).unwrap();

        assert!(!owned_temp.exists());
        assert_eq!(fs::read(unrelated).unwrap(), b"keep");
    }

    #[cfg(unix)]
    fn create_directory_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(source, target)
    }

    #[cfg(windows)]
    fn create_directory_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(source, target)
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn rejects_symlinked_artifact_directory_outside_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        create_directory_symlink(outside.path(), &workspace.path().join(ARTIFACT_DIRECTORY))
            .expect("platform must create the directory symlink used by this security boundary test");
        let store = ArtifactStore::new(workspace.path());

        assert!(matches!(
            store.persist_inline(ArtifactKind::Image, "image/png", ONE_PIXEL_PNG),
            Err(ArtifactStoreError::OutsideWorkspace)
        ));
    }
}

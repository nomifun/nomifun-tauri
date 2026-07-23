//! Dataset-generation lifecycle and factory reset coordination.
//!
//! The reset coordinator deliberately lives outside the database crate.  It
//! owns the filesystem boundary (database family, managed side stores and the
//! generation receipt), while the database worker remains responsible for
//! probing/proving the database contract.  This keeps the destructive
//! transition before a writable `SqlitePool` exists.
//!
//! A reset is a durable, resumable move operation:
//!
//! 1. write an immutable plan and an `armed` phase;
//! 2. move every known managed root into a fixed retired-dataset directory;
//! 3. install one new generation;
//! 4. let database bootstrap create/prove the new database;
//! 5. write the v3 receipt and remove the pending plan.
//!
//! A crash at any point leaves the plan and fixed destinations in place.
//! Retry accepts only the source-present/destination-absent or
//! source-absent/destination-present states. It never walks or deletes an
//! arbitrary external workspace; the one resolved product-managed
//! `<work_dir>/conversations` root is explicitly part of the dataset and is
//! moved as a single root when it lives outside `data_dir`.

use std::fs::{self, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::dataset_roots::{DatasetRootKind, managed_dataset_roots};
use crate::error::AppError;
use crate::id::validate_uuidv7;
use crate::timestamp::now_ms;

/// Current v3 explicit-reset request. It is a control-plane request, not a
/// historical dataset format, and is parsed strictly.
pub const V3_DATASET_RESET_REQUEST_FILE: &str = ".dataset-v3-reset.request.json";

/// Durable v3 automatic-reset plan directory.
pub const V3_DATASET_RESET_DIR: &str = ".id-reference-v3-dataset-reset.pending";
pub const V3_DATASET_RESET_PLAN_FILE: &str = "plan.json";
pub const V3_DATASET_RECEIPT_FILE: &str = "dataset-v3.json";
pub const V3_DATASET_BOOTSTRAP_FILE: &str = ".dataset-v3.bootstrap.json";
pub const V3_DATASET_CONTRACT_VERSION: u32 = 3;
pub const RETIRED_DATASETS_DIR: &str = "retired-datasets";
const WORK_RETIRED_DATASETS_DIR: &str = ".nomifun-retired-datasets";
const MANAGED_WORKSPACES_DIR: &str = "conversations";

const PLAN_VERSION: u32 = 1;
const DB_FILE: &str = "nomifun-backend.db";
const STORAGE_GENERATION_FILE: &str = "storage-generation";
const RETIRED_FACTORY_RESET_MARKER: &str = "factory-reset.pending";
// Order is deliberate: retire every sidecar/lock before the main database.
// A crash before the final rename therefore cannot leave a retired main file
// next to an active stale WAL/SHM family.
const DB_FAMILY: &[&str] = &[
    "nomifun-backend.db-wal",
    "nomifun-backend.db-shm",
    "nomifun-backend.db-journal",
    "nomifun-backend.db.migrate.lock",
    "nomifun-backend.db",
];

fn managed_roots() -> impl Iterator<Item = (&'static str, ManagedRootKind)> {
    // Unknown files/directories are not recursively swept: logs, runtime
    // locks, developer caches and arbitrary user files must survive a reset.
    DB_FAMILY
        .iter()
        .copied()
        .map(|path| (path, ManagedRootKind::File))
        .chain([
            (STORAGE_GENERATION_FILE, ManagedRootKind::File),
            (V3_DATASET_RECEIPT_FILE, ManagedRootKind::File),
            (V3_DATASET_BOOTSTRAP_FILE, ManagedRootKind::File),
            // Retire the pre-v3 control artifact with its dataset; it is never
            // parsed as a compatibility request.
            (RETIRED_FACTORY_RESET_MARKER, ManagedRootKind::File),
        ])
        .chain(managed_dataset_roots().map(|root| {
            (
                root.path,
                match root.kind {
                    DatasetRootKind::File => ManagedRootKind::File,
                    DatasetRootKind::Directory => ManagedRootKind::Directory,
                },
            )
        }))
}

/// Fixed-shape v3 explicit-reset request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DatasetResetRequest {
    pub version: u32,
    pub operation_id: String,
    pub requested_at: i64,
}

impl DatasetResetRequest {
    fn new() -> Self {
        Self {
            version: PLAN_VERSION,
            operation_id: Uuid::now_v7().to_string(),
            requested_at: now_ms(),
        }
    }

    fn validate(&self) -> Result<(), AppError> {
        if self.version != PLAN_VERSION {
            return Err(AppError::Internal(format!(
                "unsupported v3 dataset reset request version {}",
                self.version
            )));
        }
        validate_uuidv7(&self.operation_id).map_err(|error| {
            AppError::Internal(format!(
                "invalid v3 dataset reset request operation_id: {error}"
            ))
        })?;
        if self.requested_at <= 0 {
            return Err(AppError::Internal(
                "v3 dataset reset request requested_at must be positive".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetResetReason {
    NonV3Dataset,
    ExplicitFactoryReset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRootBase {
    DataDir,
    WorkDir,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRootKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedRootPlan {
    pub base: ManagedRootBase,
    pub relative_path: String,
    pub retired_relative_path: String,
    pub kind: ManagedRootKind,
    pub initially_present: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetResetPlan {
    pub version: u32,
    pub operation_id: String,
    pub reason: DatasetResetReason,
    pub data_dir: String,
    pub work_dir: String,
    pub generation: String,
    pub retired_dir: String,
    pub work_retired_dir: String,
    pub requested_at: i64,
    pub roots: Vec<ManagedRootPlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetReceipt {
    pub contract_version: u32,
    pub generation: String,
    /// Canonical resolved work root that belongs to this dataset generation.
    ///
    /// This is deliberately part of the receipt rather than inferred from the
    /// current process configuration.  A database receipt must never make an
    /// unrelated `<work_dir>/conversations` tree look like part of the same
    /// dataset merely because the operator changed `--work-dir`.
    pub work_root: String,
    pub installed_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DatasetBootstrapBinding {
    contract_version: u32,
    generation: String,
    work_root: String,
    prepared_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatasetReceiptStatus {
    Missing,
    Current,
    WorkRootMismatch,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatasetPreparation {
    Unchanged,
    ResetApplied,
}

fn request_path(data_dir: &Path) -> PathBuf {
    data_dir.join(V3_DATASET_RESET_REQUEST_FILE)
}

fn reset_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(V3_DATASET_RESET_DIR)
}

fn plan_path(data_dir: &Path) -> PathBuf {
    reset_dir(data_dir).join(V3_DATASET_RESET_PLAN_FILE)
}

fn phase_path(data_dir: &Path, phase: &str) -> PathBuf {
    reset_dir(data_dir).join(format!("phase-{phase}"))
}

fn receipt_path(data_dir: &Path) -> PathBuf {
    data_dir.join(V3_DATASET_RECEIPT_FILE)
}

fn bootstrap_binding_path(data_dir: &Path) -> PathBuf {
    data_dir.join(V3_DATASET_BOOTSTRAP_FILE)
}

fn canonical_data_dir(data_dir: &Path) -> Result<PathBuf, AppError> {
    let metadata = fs::symlink_metadata(data_dir).map_err(|error| {
        AppError::Internal(format!(
            "inspect dataset root {}: {error}",
            data_dir.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::Internal(format!(
            "dataset root must be a real directory: {}",
            data_dir.display()
        )));
    }
    fs::canonicalize(data_dir).map_err(|error| {
        AppError::Internal(format!(
            "canonicalize dataset root {}: {error}",
            data_dir.display()
        ))
    })
}

fn canonical_work_dir(work_dir: &Path) -> Result<PathBuf, AppError> {
    ensure_real_directory(work_dir, "managed work directory")?;
    canonical_existing_work_dir(work_dir)
}

fn canonical_existing_work_dir(work_dir: &Path) -> Result<PathBuf, AppError> {
    let metadata = fs::symlink_metadata(work_dir).map_err(|error| {
        AppError::Internal(format!(
            "inspect managed work directory {}: {error}",
            work_dir.display()
        ))
    })?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(AppError::Internal(format!(
            "managed work directory must be a real directory: {}",
            work_dir.display()
        )));
    }
    fs::canonicalize(work_dir).map_err(|error| {
        AppError::Internal(format!(
            "canonicalize managed work directory {}: {error}",
            work_dir.display()
        ))
    })
}

fn validate_relative_path(value: &str) -> Result<(), AppError> {
    let path = Path::new(value);
    if value.is_empty() || path.is_absolute() {
        return Err(AppError::Internal(format!(
            "dataset reset path is not relative: {value:?}"
        )));
    }
    for component in path.components() {
        if matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_)) {
            return Err(AppError::Internal(format!(
                "dataset reset path escapes its root: {value:?}"
            )));
        }
    }
    Ok(())
}

fn relative_path_is_under(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root).is_ok()
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn ensure_real_directory(path: &Path, description: &str) -> Result<(), AppError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() => {
            Err(AppError::Internal(format!(
                "{description} must be a real directory: {}",
                path.display()
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|create_error| {
                AppError::Internal(format!(
                    "create {description} {}: {create_error}",
                    path.display()
                ))
            })?;
            let metadata = fs::symlink_metadata(path).map_err(|inspect_error| {
                AppError::Internal(format!(
                    "inspect created {description} {}: {inspect_error}",
                    path.display()
                ))
            })?;
            if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                return Err(AppError::Internal(format!(
                    "created {description} is not a real directory: {}",
                    path.display()
                )));
            }
            Ok(())
        }
        Err(error) => Err(AppError::Internal(format!(
            "inspect {description} {}: {error}",
            path.display()
        ))),
    }
}

fn ensure_safe_destination_parent(
    destination: &Path,
    retired_root: &Path,
) -> Result<(), AppError> {
    let parent = destination.parent().ok_or_else(|| {
        AppError::Internal(format!(
            "reset destination has no parent: {}",
            destination.display()
        ))
    })?;
    if !relative_path_is_under(parent, retired_root) {
        return Err(AppError::Internal(format!(
            "reset destination parent escapes retired root: {}",
            parent.display()
        )));
    }

    // Check every existing component.  `create_dir_all` follows a symlink, so
    // checking only the leaf would permit a malicious/stale junction in the
    // quarantine tree to redirect a move outside the data directory.
    let mut current = retired_root.to_path_buf();
    let relative = parent.strip_prefix(retired_root).map_err(|_| {
        AppError::Internal("reset destination is outside retired root".into())
    })?;
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(AppError::Internal(format!(
                "reset destination parent contains unsafe component: {}",
                parent.display()
            )));
        }
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() => {
                return Err(AppError::Internal(format!(
                    "reset destination parent is not a real directory: {}",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|create_error| {
                    AppError::Internal(format!(
                        "create reset destination parent {}: {create_error}",
                        current.display()
                    ))
                })?;
            }
            Err(error) => {
                return Err(AppError::Internal(format!(
                    "inspect reset destination parent {}: {error}",
                    current.display()
                )));
            }
        }
    }
    Ok(())
}

fn validate_root_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    kind: ManagedRootKind,
) -> Result<(), AppError> {
    if metadata_is_link_or_reparse(metadata)
        || match kind {
            ManagedRootKind::File => !metadata.is_file(),
            ManagedRootKind::Directory => !metadata.is_dir(),
        }
    {
        return Err(AppError::Internal(format!(
            "managed reset root has the wrong type or symlink/reparse indirection: {}",
            path.display()
        )));
    }
    Ok(())
}

fn inspect_planned_root(path: &Path, kind: ManagedRootKind) -> Result<bool, AppError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_root_metadata(path, &metadata, kind)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AppError::Internal(format!(
            "inspect managed reset root {}: {error}",
            path.display()
        ))),
    }
}

fn validate_plan(
    plan: &DatasetResetPlan,
    data_dir: &Path,
    work_dir: &Path,
) -> Result<(), AppError> {
    if plan.version != PLAN_VERSION {
        return Err(AppError::Internal(format!(
            "unsupported v3 dataset reset plan version {}",
            plan.version
        )));
    }
    validate_uuidv7(&plan.operation_id).map_err(|error| {
        AppError::Internal(format!(
            "v3 dataset reset operation ID is invalid: {} ({error})",
            plan.operation_id
        ))
    })?;
    validate_uuidv7(&plan.generation).map_err(|error| {
        AppError::Internal(format!(
            "v3 dataset reset generation is invalid: {} ({error})",
            plan.generation
        ))
    })?;
    if plan.data_dir != canonical_data_dir(data_dir)?.display().to_string() {
        return Err(AppError::Internal(
            "v3 dataset reset plan belongs to a different data directory".into(),
        ));
    }
    if plan.work_dir
        != canonical_existing_work_dir(work_dir)?
            .display()
            .to_string()
    {
        return Err(AppError::Internal(
            "v3 dataset reset plan belongs to a different managed work directory".into(),
        ));
    }
    if plan.requested_at <= 0 {
        return Err(AppError::Internal(
            "v3 dataset reset plan requested_at must be positive".into(),
        ));
    }
    validate_relative_path(&plan.retired_dir)?;
    if !plan.retired_dir.starts_with(&format!("{RETIRED_DATASETS_DIR}/")) {
        return Err(AppError::Internal(
            "v3 dataset reset retired directory is outside retired-datasets".into(),
        ));
    }
    validate_relative_path(&plan.work_retired_dir)?;
    if !plan
        .work_retired_dir
        .starts_with(&format!("{WORK_RETIRED_DATASETS_DIR}/"))
    {
        return Err(AppError::Internal(
            "v3 dataset reset managed-workspace destination is outside its retired root".into(),
        ));
    }
    if plan.roots.is_empty() {
        return Err(AppError::Internal(
            "v3 dataset reset plan contains no managed roots".into(),
        ));
    }
    for root in &plan.roots {
        validate_relative_path(&root.relative_path)?;
        validate_relative_path(&root.retired_relative_path)?;
        if root.base == ManagedRootBase::DataDir
            && (root.relative_path == V3_DATASET_RESET_DIR
                || root.relative_path == RETIRED_DATASETS_DIR)
        {
            return Err(AppError::Internal(
                "v3 dataset reset plan attempts to move its own control directory".into(),
            ));
        }
        let expected_retired_root = match root.base {
            ManagedRootBase::DataDir => &plan.retired_dir,
            ManagedRootBase::WorkDir => &plan.work_retired_dir,
        };
        if !root
            .retired_relative_path
            .starts_with(&format!("{expected_retired_root}/"))
        {
            return Err(AppError::Internal(
                "v3 dataset reset root destination is outside its retired directory".into(),
            ));
        }
    }

    let canonical = canonical_data_dir(data_dir)?;
    let canonical_work = canonical_existing_work_dir(work_dir)?;
    let mut expected_roots: Vec<_> = managed_roots()
        .map(|(path, kind)| {
            (
                ManagedRootBase::DataDir,
                path,
                kind,
                format!("{}/{}", plan.retired_dir, path),
            )
        })
        .collect();
    if canonical_work != canonical {
        expected_roots.push((
            ManagedRootBase::WorkDir,
            MANAGED_WORKSPACES_DIR,
            ManagedRootKind::Directory,
            format!("{}/{}", plan.work_retired_dir, MANAGED_WORKSPACES_DIR),
        ));
    }
    if plan.roots.len() != expected_roots.len()
        || plan
            .roots
            .iter()
            .zip(expected_roots.iter())
            .any(|(actual, expected)| {
                actual.base != expected.0
                    || actual.relative_path != expected.1
                    || actual.kind != expected.2
                    || actual.retired_relative_path != expected.3
            })
    {
        return Err(AppError::Internal(
            "v3 dataset reset plan managed-root registry does not match this build".into(),
        ));
    }
    let retired_root = canonical.join(&plan.retired_dir);
    if let Ok(metadata) = fs::symlink_metadata(&canonical.join(RETIRED_DATASETS_DIR)) {
        if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
            return Err(AppError::Internal(
                "retired-datasets must be a real directory".into(),
            ));
        }
    }
    if let Ok(metadata) = fs::symlink_metadata(&retired_root)
        && (metadata_is_link_or_reparse(&metadata) || !metadata.is_dir())
    {
        return Err(AppError::Internal(
            "retired dataset generation directory must be a real directory".into(),
        ));
    }
    let work_retired_root = canonical_work.join(&plan.work_retired_dir);
    if let Ok(metadata) = fs::symlink_metadata(&work_retired_root)
        && (metadata_is_link_or_reparse(&metadata) || !metadata.is_dir())
    {
        return Err(AppError::Internal(
            "managed-workspace retired dataset directory must be a real directory".into(),
        ));
    }
    Ok(())
}

fn sync_parent(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    sync_directory(parent)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    OpenOptions::new().read(true).open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    // Windows does not support opening a directory with ordinary
    // `CreateFile` flags through `std::fs::OpenOptions`.  Directory metadata
    // is nevertheless protected by the atomic rename itself; use a no-op for
    // the directory fsync step while still syncing every written file.
    let _ = path;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    let _ = path;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("state"),
        Uuid::now_v7()
    ));
    {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        use std::io::Write;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    sync_parent(path)
}

fn write_phase(data_dir: &Path, phase: &str) -> Result<(), AppError> {
    let path = phase_path(data_dir, phase);
    if path.exists() {
        return Ok(());
    }
    write_atomic(&path, b"v1\n")
        .map_err(|error| AppError::Internal(format!("write reset phase {phase}: {error}")))
}

fn has_phase(data_dir: &Path, phase: &str) -> bool {
    matches!(
        fs::symlink_metadata(phase_path(data_dir, phase)),
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink()
    )
}

fn remove_reset_dir(data_dir: &Path) -> Result<(), AppError> {
    let path = reset_dir(data_dir);
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        AppError::Internal(format!(
            "inspect completed v3 dataset reset plan {}: {error}",
            path.display()
        ))
    })?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(AppError::Internal(format!(
            "refusing to remove non-directory v3 dataset reset control path {}",
            path.display()
        )));
    }
    remove_path_with_retry(&path).map_err(|error| {
        AppError::Internal(format!(
            "remove completed v3 dataset reset plan {}: {error}",
            path.display()
        ))
    })
}

/// Arm an immutable v3 reset plan.  If a plan already exists, it is validated
/// and returned; no new generation or destination is minted on retry.
pub fn arm_v3_dataset_reset(
    data_dir: &Path,
    work_dir: &Path,
    reason: DatasetResetReason,
) -> Result<DatasetResetPlan, AppError> {
    if let Some(existing) = read_pending_v3_reset(data_dir, work_dir)? {
        if !has_phase(data_dir, "armed") {
            return Err(AppError::Internal(
                "v3 dataset reset plan exists without its armed phase".into(),
            ));
        }
        clear_v3_dataset_reset_request(data_dir)?;
        return Ok(existing);
    }

    let canonical = canonical_data_dir(data_dir)?;
    let canonical_work = canonical_work_dir(work_dir)?;
    let generation = Uuid::now_v7().to_string();
    let operation_id = Uuid::now_v7().to_string();
    let retired_dir = format!("{RETIRED_DATASETS_DIR}/id-reference-v3-{generation}");
    let work_retired_dir =
        format!("{WORK_RETIRED_DATASETS_DIR}/id-reference-v3-{generation}");
    let mut roots = managed_roots()
        .map(|(relative_path, kind)| {
            let source = canonical.join(relative_path);
            let initially_present = inspect_planned_root(&source, kind)?;
            Ok(ManagedRootPlan {
                base: ManagedRootBase::DataDir,
                relative_path: relative_path.to_owned(),
                retired_relative_path: format!("{retired_dir}/{relative_path}"),
                kind,
                initially_present,
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    if canonical_work != canonical {
        let source = canonical_work.join(MANAGED_WORKSPACES_DIR);
        roots.push(ManagedRootPlan {
            base: ManagedRootBase::WorkDir,
            relative_path: MANAGED_WORKSPACES_DIR.to_owned(),
            retired_relative_path: format!("{work_retired_dir}/{MANAGED_WORKSPACES_DIR}"),
            kind: ManagedRootKind::Directory,
            initially_present: inspect_planned_root(&source, ManagedRootKind::Directory)?,
        });
    }
    let plan = DatasetResetPlan {
        version: PLAN_VERSION,
        operation_id,
        reason,
        data_dir: canonical.display().to_string(),
        work_dir: canonical_work.display().to_string(),
        generation,
        retired_dir,
        work_retired_dir,
        requested_at: now_ms(),
        roots,
    };
    let bytes = serde_json::to_vec_pretty(&plan)
        .map_err(|error| AppError::Internal(format!("serialize v3 reset plan: {error}")))?;
    ensure_real_directory(&reset_dir(data_dir), "v3 reset plan directory")?;
    write_atomic(&plan_path(data_dir), &bytes)
        .map_err(|error| AppError::Internal(format!("write v3 reset plan: {error}")))?;
    write_phase(data_dir, "armed")?;
    validate_plan(&plan, data_dir, work_dir)?;

    // The request has now been durably superseded by the immutable plan. A
    // crash after this point is recovered from the plan.
    clear_v3_dataset_reset_request(data_dir)?;
    Ok(plan)
}

pub fn read_pending_v3_reset(
    data_dir: &Path,
    work_dir: &Path,
) -> Result<Option<DatasetResetPlan>, AppError> {
    let path = plan_path(data_dir);
    match fs::read(&path) {
        Ok(bytes) => {
            let plan: DatasetResetPlan = serde_json::from_slice(&bytes).map_err(|error| {
                AppError::Internal(format!(
                    "malformed v3 dataset reset plan {}: {error}",
                    path.display()
                ))
            })?;
            validate_plan(&plan, data_dir, work_dir)?;
            Ok(Some(plan))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::Internal(format!(
            "read v3 dataset reset plan {}: {error}",
            path.display()
        ))),
    }
}

fn install_generation(data_dir: &Path, generation: &str) -> Result<(), AppError> {
    let path = data_dir.join(STORAGE_GENERATION_FILE);
    match fs::read_to_string(&path) {
        Ok(existing) if existing == generation => return Ok(()),
        Ok(_) => {
            return Err(AppError::Internal(format!(
                "storage-generation has an unexpected value at {}",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AppError::Internal(format!(
                "read storage-generation {}: {error}",
                path.display()
            )));
        }
    }
    write_atomic(path.as_path(), generation.as_bytes()).map_err(|error| {
        AppError::Internal(format!("install new storage generation: {error}"))
    })
}

/// Apply the pending filesystem transition.  The plan remains until database
/// bootstrap has completed and [`finalize_v3_dataset_reset`] is called.
pub fn apply_pending_v3_dataset_reset(
    data_dir: &Path,
    work_dir: &Path,
) -> Result<bool, AppError> {
    let Some(plan) = read_pending_v3_reset(data_dir, work_dir)? else {
        return Ok(false);
    };
    if !has_phase(data_dir, "armed") {
        return Err(AppError::Internal(
            "v3 dataset reset is not armed; refusing mutation".into(),
        ));
    }

    let retired_root = data_dir.join(&plan.retired_dir);
    let canonical_work = canonical_work_dir(work_dir)?;
    let work_retired_root = canonical_work.join(&plan.work_retired_dir);
    ensure_real_directory(
        &data_dir.join(RETIRED_DATASETS_DIR),
        "retired-datasets directory",
    )?;
    ensure_real_directory(&retired_root, "retired dataset generation directory")?;
    if plan
        .roots
        .iter()
        .any(|root| root.base == ManagedRootBase::WorkDir)
    {
        ensure_real_directory(
            &canonical_work.join(WORK_RETIRED_DATASETS_DIR),
            "managed-workspace retired-datasets directory",
        )?;
        ensure_real_directory(
            &work_retired_root,
            "managed-workspace retired dataset generation directory",
        )?;
    }
    write_phase(data_dir, "quarantine-started")?;

    for root in &plan.roots {
        let (base, retired_base) = match root.base {
            ManagedRootBase::DataDir => (data_dir, retired_root.as_path()),
            ManagedRootBase::WorkDir => (canonical_work.as_path(), work_retired_root.as_path()),
        };
        let source = base.join(&root.relative_path);
        let destination = base.join(&root.retired_relative_path);
        ensure_safe_destination_parent(&destination, retired_base)?;
        let source_state = fs::symlink_metadata(&source);
        let destination_state = fs::symlink_metadata(&destination);
        let generation_installed = has_phase(data_dir, "generation-installed");
        match (source_state, destination_state) {
            (Ok(source_metadata), Err(error))
                if root.initially_present
                    && !generation_installed
                    && error.kind() == std::io::ErrorKind::NotFound =>
            {
                validate_root_metadata(&source, &source_metadata, root.kind)?;
                rename_with_retry(&source, &destination).map_err(|error| {
                    AppError::Internal(format!(
                        "quarantine managed root {} -> {}: {error}",
                        source.display(),
                        destination.display()
                    ))
                })?;
                sync_parent(&source).map_err(|error| {
                    AppError::Internal(format!(
                        "sync source parent after quarantining {}: {error}",
                        source.display()
                    ))
                })?;
                sync_parent(&destination).map_err(|error| {
                    AppError::Internal(format!(
                        "sync retired parent after quarantining {}: {error}",
                        destination.display()
                    ))
                })?;
            }
            (Err(error), Ok(destination_metadata))
                if root.initially_present
                    && error.kind() == std::io::ErrorKind::NotFound =>
            {
                validate_root_metadata(&destination, &destination_metadata, root.kind)?;
                // Crash after the rename: the fixed destination proves this
                // root's transition is complete.
            }
            (Err(error), Err(dest_error))
                if !root.initially_present
                    && error.kind() == std::io::ErrorKind::NotFound
                    && dest_error.kind() == std::io::ErrorKind::NotFound =>
            {}
            (Ok(source_metadata), Ok(destination_metadata))
                if root.initially_present && generation_installed =>
            {
                // The generation-installed phase means the destination is the
                // retired source and the active source is a newly created v3
                // root. This is the normal crash-recovery state after DB or
                // side-store bootstrap started but before receipt/finalize.
                validate_root_metadata(&source, &source_metadata, root.kind)?;
                validate_root_metadata(&destination, &destination_metadata, root.kind)?;
            }
            (Ok(source_metadata), Err(dest_error))
                if !root.initially_present
                    && generation_installed
                    && dest_error.kind() == std::io::ErrorKind::NotFound =>
            {
                // This root did not exist in the retired dataset and was
                // created only by the fresh v3 bootstrap.
                validate_root_metadata(&source, &source_metadata, root.kind)?;
            }
            (Ok(_), Ok(_)) => {
                return Err(AppError::Internal(format!(
                    "ambiguous v3 reset root state: both {} and {} exist",
                    source.display(),
                    destination.display()
                )));
            }
            (Err(error), Err(dest_error))
                if root.initially_present
                    && error.kind() == std::io::ErrorKind::NotFound
                    && dest_error.kind() == std::io::ErrorKind::NotFound =>
            {
                return Err(AppError::Internal(format!(
                    "managed reset root disappeared from both active and retired locations: {}",
                    root.relative_path
                )));
            }
            (Err(error), Ok(_))
                if !root.initially_present
                    && error.kind() == std::io::ErrorKind::NotFound =>
            {
                return Err(AppError::Internal(format!(
                    "unexpected retired copy exists for initially absent root {}",
                    root.relative_path
                )));
            }
            (Ok(_), Err(error))
                if !generation_installed
                    && !root.initially_present
                    && error.kind() == std::io::ErrorKind::NotFound =>
            {
                return Err(AppError::Internal(format!(
                    "initially absent managed root appeared before generation installation: {}",
                    source.display()
                )));
            }
            (Err(error), _) => {
                return Err(AppError::Internal(format!(
                    "inspect reset source {}: {error}",
                    source.display()
                )));
            }
            (Ok(_), Err(error)) => {
                return Err(AppError::Internal(format!(
                    "inspect reset destination {}: {error}",
                    destination.display()
                )));
            }
        }
    }

    write_phase(data_dir, "quarantined")?;
    install_generation(data_dir, &plan.generation)?;
    write_phase(data_dir, "generation-installed")?;
    tracing::warn!(
        target: "factory_reset",
        reason = ?plan.reason,
        generation = %plan.generation,
        retired_dir = %plan.retired_dir,
        "managed dataset quarantined; awaiting fresh database bootstrap"
    );
    Ok(true)
}

/// Record the v3 receipt after the fresh database has been opened and passed
/// the database worker's contract checks.
pub fn write_v3_dataset_receipt(
    data_dir: &Path,
    generation: &str,
) -> Result<(), AppError> {
    // Keep the old API usable by restore/maintenance code that only owns a
    // data directory.  During a pending reset the immutable plan contains
    // the authoritative resolved work root, so use it when available; a
    // standalone data-only dataset naturally binds to data_dir itself.
    let work_dir = pending_plan_work_dir(data_dir)?.unwrap_or_else(|| data_dir.to_path_buf());
    write_v3_dataset_receipt_for_work_dir(data_dir, &work_dir, generation)
}

pub fn write_v3_dataset_receipt_for_work_dir(
    data_dir: &Path,
    work_dir: &Path,
    generation: &str,
) -> Result<(), AppError> {
    validate_uuidv7(generation)
        .map_err(|error| AppError::Internal(format!("invalid dataset generation: {error}")))?;
    let canonical_work = canonical_work_dir(work_dir)?;
    let receipt = DatasetReceipt {
        contract_version: V3_DATASET_CONTRACT_VERSION,
        generation: generation.to_owned(),
        work_root: canonical_work.display().to_string(),
        installed_at: now_ms(),
    };
    let bytes = serde_json::to_vec_pretty(&receipt)
        .map_err(|error| AppError::Internal(format!("serialize v3 dataset receipt: {error}")))?;
    write_atomic(&receipt_path(data_dir), &bytes)
        .map_err(|error| AppError::Internal(format!("write v3 dataset receipt: {error}")))?;
    clear_v3_dataset_bootstrap_binding(data_dir)?;
    Ok(())
}

/// Finish a reset only after the new database has been initialized.
pub fn finalize_v3_dataset_reset(
    data_dir: &Path,
    work_dir: &Path,
) -> Result<bool, AppError> {
    let Some(plan) = read_pending_v3_reset(data_dir, work_dir)? else {
        return Ok(false);
    };
    if !has_phase(data_dir, "generation-installed") {
        return Err(AppError::Internal(
            "cannot finalize v3 dataset reset before generation installation".into(),
        ));
    }
    let receipt: DatasetReceipt = serde_json::from_slice(
        &fs::read(receipt_path(data_dir)).map_err(|error| {
            AppError::Internal(format!("read v3 dataset receipt during finalize: {error}"))
        })?,
    )
    .map_err(|error| AppError::Internal(format!("invalid v3 dataset receipt: {error}")))?;
    if receipt.contract_version != V3_DATASET_CONTRACT_VERSION
        || receipt.generation != plan.generation
        || receipt.work_root != plan.work_dir
    {
        return Err(AppError::Internal(
            "v3 dataset receipt does not match the reset plan".into(),
        ));
    }
    let database = data_dir.join(DB_FILE);
    let metadata = fs::symlink_metadata(&database).map_err(|error| {
        AppError::Internal(format!(
            "fresh database missing while finalizing reset {}: {error}",
            database.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::Internal(
            "fresh database is not a regular file while finalizing reset".into(),
        ));
    }
    remove_reset_dir(data_dir)?;
    tracing::info!(
        target: "factory_reset",
        generation = %plan.generation,
        "v3 managed dataset reset finalized"
    );
    Ok(true)
}

fn pending_plan_work_dir(data_dir: &Path) -> Result<Option<PathBuf>, AppError> {
    let bytes = match fs::read(plan_path(data_dir)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AppError::Internal(format!(
                "read pending v3 reset plan for receipt: {error}"
            )));
        }
    };
    let plan: DatasetResetPlan = serde_json::from_slice(&bytes).map_err(|error| {
        AppError::Internal(format!(
            "invalid pending v3 reset plan while writing receipt: {error}"
        ))
    })?;
    let work_dir = PathBuf::from(&plan.work_dir);
    validate_plan(&plan, data_dir, &work_dir)?;
    Ok(Some(work_dir))
}

pub fn write_v3_dataset_bootstrap_binding(
    data_dir: &Path,
    work_dir: &Path,
    generation: &str,
) -> Result<(), AppError> {
    validate_uuidv7(generation)
        .map_err(|error| AppError::Internal(format!("invalid dataset generation: {error}")))?;
    let canonical_work = canonical_work_dir(work_dir)?;
    let binding = DatasetBootstrapBinding {
        contract_version: V3_DATASET_CONTRACT_VERSION,
        generation: generation.to_owned(),
        work_root: canonical_work.display().to_string(),
        prepared_at: now_ms(),
    };

    match inspect_v3_dataset_bootstrap_binding(data_dir, work_dir)? {
        DatasetReceiptStatus::Missing => {}
        DatasetReceiptStatus::Current => {
            let existing: DatasetBootstrapBinding = serde_json::from_slice(
                &fs::read(bootstrap_binding_path(data_dir)).map_err(|error| {
                    AppError::Internal(format!(
                        "read current v3 bootstrap binding {}: {error}",
                        bootstrap_binding_path(data_dir).display()
                    ))
                })?,
            )
            .map_err(|error| {
                AppError::Internal(format!("invalid current v3 bootstrap binding: {error}"))
            })?;
            if existing.generation == generation {
                return Ok(());
            }
            return Err(AppError::Internal(
                "v3 bootstrap binding generation does not match storage-generation".into(),
            ));
        }
        DatasetReceiptStatus::WorkRootMismatch => {
            return Err(AppError::Internal(
                "v3 bootstrap binding belongs to a different resolved work root".into(),
            ));
        }
        DatasetReceiptStatus::Invalid => {
            return Err(AppError::Internal(
                "v3 bootstrap binding is malformed or inconsistent".into(),
            ));
        }
    }

    let bytes = serde_json::to_vec_pretty(&binding).map_err(|error| {
        AppError::Internal(format!("serialize v3 dataset bootstrap binding: {error}"))
    })?;
    write_atomic(&bootstrap_binding_path(data_dir), &bytes).map_err(|error| {
        AppError::Internal(format!("write v3 dataset bootstrap binding: {error}"))
    })
}

pub fn inspect_v3_dataset_bootstrap_binding(
    data_dir: &Path,
    work_dir: &Path,
) -> Result<DatasetReceiptStatus, AppError> {
    let bytes = match fs::read(bootstrap_binding_path(data_dir)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DatasetReceiptStatus::Missing);
        }
        Err(error) => {
            return Err(AppError::Internal(format!(
                "read v3 dataset bootstrap binding {}: {error}",
                bootstrap_binding_path(data_dir).display()
            )));
        }
    };
    let Ok(binding) = serde_json::from_slice::<DatasetBootstrapBinding>(&bytes) else {
        return Ok(DatasetReceiptStatus::Invalid);
    };
    if binding.contract_version != V3_DATASET_CONTRACT_VERSION
        || validate_uuidv7(&binding.generation).is_err()
        || !matches!(
            fs::read_to_string(data_dir.join(STORAGE_GENERATION_FILE)),
            Ok(generation) if generation == binding.generation
        )
    {
        return Ok(DatasetReceiptStatus::Invalid);
    }
    let canonical_work = canonical_existing_work_dir(work_dir)?;
    if binding.work_root != canonical_work.display().to_string() {
        return Ok(DatasetReceiptStatus::WorkRootMismatch);
    }
    Ok(DatasetReceiptStatus::Current)
}

fn clear_v3_dataset_bootstrap_binding(data_dir: &Path) -> Result<(), AppError> {
    let path = bootstrap_binding_path(data_dir);
    match fs::remove_file(&path) {
        Ok(()) => sync_parent(&path).map_err(|error| {
            AppError::Internal(format!(
                "sync v3 dataset bootstrap binding removal {}: {error}",
                path.display()
            ))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::Internal(format!(
            "remove v3 dataset bootstrap binding {}: {error}",
            path.display()
        ))),
    }
}

pub fn inspect_v3_dataset_receipt(
    data_dir: &Path,
    work_dir: &Path,
) -> Result<DatasetReceiptStatus, AppError> {
    let bytes = match fs::read(receipt_path(data_dir)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DatasetReceiptStatus::Missing);
        }
        Err(error) => {
            return Err(AppError::Internal(format!(
                "read v3 dataset receipt {}: {error}",
                receipt_path(data_dir).display()
            )));
        }
    };
    let Ok(receipt) = serde_json::from_slice::<DatasetReceipt>(&bytes) else {
        return Ok(DatasetReceiptStatus::Invalid);
    };
    if receipt.contract_version != V3_DATASET_CONTRACT_VERSION
        || validate_uuidv7(&receipt.generation).is_err()
    {
        return Ok(DatasetReceiptStatus::Invalid);
    }
    if !matches!(
        fs::read_to_string(data_dir.join(STORAGE_GENERATION_FILE)),
        Ok(generation) if generation == receipt.generation
    ) || !matches!(
        fs::symlink_metadata(data_dir.join(DB_FILE)),
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink()
    ) {
        return Ok(DatasetReceiptStatus::Invalid);
    }
    let canonical_work = canonical_existing_work_dir(work_dir)?;
    if receipt.work_root != canonical_work.display().to_string() {
        return Ok(DatasetReceiptStatus::WorkRootMismatch);
    }
    Ok(DatasetReceiptStatus::Current)
}

fn receipt_is_current(data_dir: &Path, work_dir: &Path) -> Result<bool, AppError> {
    Ok(matches!(
        inspect_v3_dataset_receipt(data_dir, work_dir)?,
        DatasetReceiptStatus::Current
    ))
}

/// Require a fully finalized current-generation v3 dataset without mutating it.
///
/// Offline preservation commands use this gate instead of
/// [`prepare_v3_dataset`]: backup must never create lifecycle markers, reset
/// old data, or capture a half-initialized post-reset dataset.
pub fn require_current_v3_dataset(data_dir: &Path) -> Result<(), AppError> {
    let receipt_bytes = fs::read(receipt_path(data_dir)).map_err(|error| {
        AppError::Internal(format!(
            "read v3 dataset receipt {}: {error}",
            receipt_path(data_dir).display()
        ))
    })?;
    let receipt: DatasetReceipt = serde_json::from_slice(&receipt_bytes)
        .map_err(|error| AppError::Internal(format!("invalid v3 dataset receipt: {error}")))?;
    require_current_v3_dataset_for_work_dir(data_dir, Path::new(&receipt.work_root))
}

pub fn require_current_v3_dataset_for_work_dir(
    data_dir: &Path,
    work_dir: &Path,
) -> Result<(), AppError> {
    match fs::symlink_metadata(request_path(data_dir)) {
        Ok(_) => {
            return Err(AppError::Internal(
                "an explicit v3 dataset reset has been requested".into(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AppError::Internal(format!(
                "inspect explicit v3 dataset reset request: {error}"
            )));
        }
    }
    match fs::symlink_metadata(reset_dir(data_dir)) {
        Ok(_) => {
            return Err(AppError::Internal(
                "a v3 dataset reset is still pending".into(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AppError::Internal(format!(
                "inspect v3 dataset reset state: {error}"
            )));
        }
    }
    if !receipt_is_current(data_dir, work_dir)? {
        return Err(AppError::Internal(
            "dataset does not have a matching finalized v3 receipt, generation, and work root"
                .into(),
        ));
    }
    Ok(())
}

/// Return whether the resolved external work root contains the product-owned
/// conversation workspace.  If a receipt is absent after a crash, this lets
/// the application distinguish a recoverable same-root bootstrap from an
/// ambiguous database/workspace pairing and fail closed without destroying a
/// database that may already be a valid v3 database.
pub fn external_work_root_has_managed_workspace(
    data_dir: &Path,
    work_dir: &Path,
) -> Result<bool, AppError> {
    let canonical_data = canonical_data_dir(data_dir)?;
    let canonical_work = canonical_work_dir(work_dir)?;
    if canonical_data == canonical_work {
        return Ok(false);
    }
    inspect_planned_root(
        &canonical_work.join(MANAGED_WORKSPACES_DIR),
        ManagedRootKind::Directory,
    )
}

fn empty_bootstrap_root(data_dir: &Path, work_dir: &Path) -> Result<bool, AppError> {
    if managed_roots().any(|(root, _)| {
        matches!(
            fs::symlink_metadata(data_dir.join(root)),
            Ok(_)
        )
    }) {
        return Ok(false);
    }

    // A relocated conversation workspace remains part of the managed dataset
    // even when data_dir itself is otherwise empty. Treating this as a fresh
    // bootstrap would leave pre-v3 workspaces active beside a new v3 database.
    let canonical_data = canonical_data_dir(data_dir)?;
    let canonical_work = canonical_work_dir(work_dir)?;
    if canonical_work != canonical_data
        && inspect_planned_root(
            &canonical_work.join(MANAGED_WORKSPACES_DIR),
            ManagedRootKind::Directory,
        )?
    {
        return Ok(false);
    }

    Ok(true)
}

/// Detect the filesystem-level dataset state before the database is opened.
///
/// The database worker should provide the authoritative schema/value probe.
/// This receipt is only the lifecycle hand-off: it prevents a clean fresh boot
/// from being reset again after the database worker has accepted the new
/// dataset.
pub fn prepare_v3_dataset(
    data_dir: &Path,
    work_dir: &Path,
) -> Result<DatasetPreparation, AppError> {
    if read_pending_v3_reset(data_dir, work_dir)?.is_some() {
        // The immutable plan is authoritative. A crash may have happened after
        // the plan commit but before the transient request was removed.
        clear_v3_dataset_reset_request(data_dir)?;
        apply_pending_v3_dataset_reset(data_dir, work_dir)?;
        return Ok(DatasetPreparation::ResetApplied);
    }
    if read_v3_dataset_reset_request(data_dir)?.is_some() {
        arm_v3_dataset_reset(data_dir, work_dir, DatasetResetReason::ExplicitFactoryReset)?;
        apply_pending_v3_dataset_reset(data_dir, work_dir)?;
        return Ok(DatasetPreparation::ResetApplied);
    }
    // A database file is the point at which the application probe becomes
    // authoritative.  Do not retire it merely because a receipt is missing,
    // stale, or was written by an older process: a perfectly valid v3
    // database can exist after a crash before receipt finalization.  The app
    // probes this file read-only and only then calls
    // `retire_non_v3_dataset_after_probe` for a rejected lineage.
    if matches!(
        fs::symlink_metadata(data_dir.join(DB_FILE)),
        Ok(_)
    ) {
        return Ok(DatasetPreparation::Unchanged);
    }
    let bootstrap_status = inspect_v3_dataset_bootstrap_binding(data_dir, work_dir)?;
    if bootstrap_status == DatasetReceiptStatus::WorkRootMismatch {
        return Err(AppError::Internal(
            "v3 bootstrap binding belongs to a different resolved work root".into(),
        ));
    }
    if receipt_is_current(data_dir, work_dir)?
        || bootstrap_status == DatasetReceiptStatus::Current
        || empty_bootstrap_root(data_dir, work_dir)?
    {
        return Ok(DatasetPreparation::Unchanged);
    }

    arm_v3_dataset_reset(data_dir, work_dir, DatasetResetReason::NonV3Dataset)?;
    apply_pending_v3_dataset_reset(data_dir, work_dir)?;
    Ok(DatasetPreparation::ResetApplied)
}

/// Retire an active dataset after a read-only database probe proved that its
/// claimed v3 receipt does not match the database identity/schema.
///
/// This is deliberately separate from [`prepare_v3_dataset`]. The filesystem
/// coordinator cannot prove SQLite lineage itself, while the application must
/// not open the rejected database through writable initialization merely to
/// discover that the receipt was forged or stale.
pub fn retire_non_v3_dataset_after_probe(
    data_dir: &Path,
    work_dir: &Path,
) -> Result<DatasetPreparation, AppError> {
    if read_pending_v3_reset(data_dir, work_dir)?.is_some() {
        return Err(AppError::Internal(
            "the active database failed its v3 probe while a dataset reset is already pending"
                .into(),
        ));
    }

    let reason = if read_v3_dataset_reset_request(data_dir)?.is_some() {
        DatasetResetReason::ExplicitFactoryReset
    } else {
        DatasetResetReason::NonV3Dataset
    };
    arm_v3_dataset_reset(data_dir, work_dir, reason)?;
    apply_pending_v3_dataset_reset(data_dir, work_dir)?;
    Ok(DatasetPreparation::ResetApplied)
}

/// Arm an explicit v3 reset request. The destructive transition occurs during
/// the next pre-database boot so it cannot race live pools or background jobs.
pub fn request_v3_dataset_reset(data_dir: &Path) -> Result<(), AppError> {
    let request = DatasetResetRequest::new();
    let json = serde_json::to_vec_pretty(&request).map_err(|error| {
        AppError::Internal(format!("serialize v3 dataset reset request: {error}"))
    })?;
    write_atomic(&request_path(data_dir), &json)
        .map_err(|error| AppError::Internal(format!("write v3 dataset reset request: {error}")))
}

fn read_v3_dataset_reset_request(
    data_dir: &Path,
) -> Result<Option<DatasetResetRequest>, AppError> {
    let path = request_path(data_dir);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => {
            return Err(AppError::Internal(format!(
                "read v3 dataset reset request {}: {error}",
                path.display()
            )));
        }
    };
    let request: DatasetResetRequest = serde_json::from_slice(&bytes).map_err(|error| {
        AppError::Internal(format!(
            "malformed v3 dataset reset request {}: {error}",
            path.display()
        ))
    })?;
    request.validate()?;
    Ok(Some(request))
}

fn clear_v3_dataset_reset_request(data_dir: &Path) -> Result<(), AppError> {
    let path = request_path(data_dir);
    match fs::remove_file(&path) {
        Ok(()) => sync_parent(&path).map_err(|error| {
            AppError::Internal(format!(
                "sync v3 dataset reset request removal {}: {error}",
                path.display()
            ))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::Internal(format!(
            "remove v3 dataset reset request {}: {error}",
            path.display()
        ))),
    }
}

fn rename_with_retry(source: &Path, destination: &Path) -> std::io::Result<()> {
    const MAX_ATTEMPTS: u32 = 5;
    for attempt in 1..=MAX_ATTEMPTS {
        match fs::rename(source, destination) {
            Ok(()) => return Ok(()),
            Err(error)
                if attempt < MAX_ATTEMPTS
                    && matches!(error.raw_os_error(), Some(5) | Some(32) | Some(33)) =>
            {
                std::thread::sleep(Duration::from_millis(80 * u64::from(attempt)));
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("rename retry loop returns on every iteration")
}

fn remove_path_with_retry(path: &Path) -> std::io::Result<()> {
    const MAX_ATTEMPTS: u32 = 5;
    for attempt in 1..=MAX_ATTEMPTS {
        let result = match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
            Ok(_) => fs::remove_file(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error)
                if attempt < MAX_ATTEMPTS
                    && matches!(error.raw_os_error(), Some(5) | Some(32) | Some(33)) =>
            {
                std::thread::sleep(Duration::from_millis(80 * u64::from(attempt)));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        fs::write(path, b"x").unwrap();
    }

    fn seed_managed_root(data_dir: &Path, relative_path: &str, kind: ManagedRootKind) {
        let path = data_dir.join(relative_path);
        match kind {
            ManagedRootKind::File => {
                fs::create_dir_all(path.parent().expect("managed file parent")).unwrap();
                touch(&path);
            }
            ManagedRootKind::Directory => {
                fs::create_dir_all(&path).unwrap();
                touch(&path.join("sentinel"));
            }
        }
    }

    #[test]
    fn empty_root_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            prepare_v3_dataset(dir.path(), dir.path()).unwrap(),
            DatasetPreparation::Unchanged
        );
    }

    #[test]
    fn explicit_reset_quarantines_managed_roots_and_keeps_logs() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join(DB_FILE));
        touch(&dir.path().join("storage-generation"));
        fs::create_dir_all(dir.path().join("conversations")).unwrap();
        touch(&dir.path().join("conversations").join("old.txt"));
        fs::create_dir_all(dir.path().join("logs")).unwrap();
        touch(&dir.path().join("logs").join("app.log"));
        request_v3_dataset_reset(dir.path()).unwrap();

        assert_eq!(
            prepare_v3_dataset(dir.path(), dir.path()).unwrap(),
            DatasetPreparation::ResetApplied
        );
        assert!(!dir.path().join(DB_FILE).exists());
        assert!(!dir.path().join("conversations").exists());
        assert!(dir.path().join("logs/app.log").exists());
        assert!(!dir.path().join(V3_DATASET_RESET_REQUEST_FILE).exists());
        assert!(dir.path().join(V3_DATASET_RESET_DIR).exists());
        assert!(dir.path().join(RETIRED_DATASETS_DIR).is_dir());
    }

    #[test]
    fn explicit_reset_quarantines_every_registered_side_store_and_db_family_member() {
        let data = tempfile::tempdir().unwrap();
        for (relative_path, kind) in managed_roots() {
            seed_managed_root(data.path(), relative_path, kind);
        }
        fs::create_dir_all(data.path().join("logs")).unwrap();
        touch(&data.path().join("logs/app.log"));

        request_v3_dataset_reset(data.path()).unwrap();
        assert_eq!(
            prepare_v3_dataset(data.path(), data.path()).unwrap(),
            DatasetPreparation::ResetApplied
        );

        let plan = read_pending_v3_reset(data.path(), data.path())
            .unwrap()
            .expect("forced reset must leave a pending plan until bootstrap finalizes");
        let planned: std::collections::BTreeMap<_, _> = plan
            .roots
            .iter()
            .map(|root| (root.relative_path.as_str(), root))
            .collect();
        assert_eq!(
            planned.len(),
            managed_roots().count(),
            "the reset plan must cover the full managed-root registry exactly once"
        );

        for (relative_path, kind) in managed_roots() {
            let root = planned
                .get(relative_path)
                .unwrap_or_else(|| panic!("missing reset plan root {relative_path}"));
            assert!(root.initially_present, "{relative_path} was seeded");
            if relative_path == STORAGE_GENERATION_FILE {
                assert_eq!(
                    fs::read_to_string(data.path().join(relative_path)).unwrap(),
                    plan.generation,
                    "reset must replace the active storage generation"
                );
            } else {
                assert!(
                    !data.path().join(relative_path).exists(),
                    "active managed root survived forced reset: {relative_path}"
                );
            }
            let retired = data.path().join(&root.retired_relative_path);
            match kind {
                ManagedRootKind::File => {
                    assert!(retired.is_file(), "missing retired file {relative_path}")
                }
                ManagedRootKind::Directory => assert!(
                    retired.join("sentinel").is_file(),
                    "missing retired side-store payload {relative_path}"
                ),
            }
        }
        assert!(data.path().join("logs/app.log").is_file());
        assert_eq!(
            fs::read_to_string(data.path().join(STORAGE_GENERATION_FILE)).unwrap(),
            plan.generation
        );
        assert!(
            !data.path().join(V3_DATASET_RECEIPT_FILE).exists(),
            "forced reset must not publish a receipt before full bootstrap"
        );
    }

    #[test]
    fn malformed_v3_reset_request_fails_closed_without_touching_data() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(V3_DATASET_RESET_REQUEST_FILE),
            b"not json",
        )
        .unwrap();
        touch(&dir.path().join(DB_FILE));
        assert!(prepare_v3_dataset(dir.path(), dir.path()).is_err());
        assert!(dir.path().join(DB_FILE).exists());
        assert!(!dir.path().join(V3_DATASET_RESET_DIR).exists());
    }

    #[test]
    fn external_managed_work_root_is_quarantined_but_arbitrary_workspace_survives() {
        let data = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let workspace = external.path().join("conversations");
        let arbitrary = external.path().join("user-project");
        fs::create_dir_all(&workspace).unwrap();
        touch(&workspace.join("keep.txt"));
        fs::create_dir_all(&arbitrary).unwrap();
        touch(&arbitrary.join("keep.txt"));
        touch(&data.path().join(DB_FILE));
        request_v3_dataset_reset(data.path()).unwrap();

        prepare_v3_dataset(data.path(), external.path()).unwrap();
        assert!(!workspace.exists());
        assert!(arbitrary.join("keep.txt").exists());

        let plan = read_pending_v3_reset(data.path(), external.path())
            .unwrap()
            .unwrap();
        assert!(
            external
                .path()
                .join(&plan.work_retired_dir)
                .join(MANAGED_WORKSPACES_DIR)
                .join("keep.txt")
                .exists()
        );
    }

    #[test]
    fn database_with_retired_factory_reset_marker_waits_for_probe() {
        let data = tempfile::tempdir().unwrap();
        touch(&data.path().join(DB_FILE));
        fs::write(
            data.path().join(RETIRED_FACTORY_RESET_MARKER),
            b"arbitrary pre-v3 bytes",
        )
        .unwrap();

        assert_eq!(
            prepare_v3_dataset(data.path(), data.path()).unwrap(),
            DatasetPreparation::Unchanged,
            "a present database must be classified by the app probe before retirement"
        );
        assert!(data.path().join(DB_FILE).is_file());
        assert!(data.path().join(RETIRED_FACTORY_RESET_MARKER).is_file());
        assert!(!data.path().join(V3_DATASET_RESET_DIR).exists());

        assert_eq!(
            retire_non_v3_dataset_after_probe(data.path(), data.path()).unwrap(),
            DatasetPreparation::ResetApplied
        );
        let plan = read_pending_v3_reset(data.path(), data.path())
            .unwrap()
            .unwrap();
        assert_eq!(plan.reason, DatasetResetReason::NonV3Dataset);
        assert!(
            data.path()
                .join(&plan.retired_dir)
                .join(RETIRED_FACTORY_RESET_MARKER)
                .is_file()
        );
    }

    #[test]
    fn pending_plan_resumes_after_source_was_already_moved() {
        let data = tempfile::tempdir().unwrap();
        touch(&data.path().join(DB_FILE));
        let plan = arm_v3_dataset_reset(
            data.path(),
            data.path(),
            DatasetResetReason::NonV3Dataset,
        )
        .unwrap();
        let source = data.path().join(DB_FILE);
        let destination = data.path().join(
            &plan
                .roots
                .iter()
                .find(|root| root.relative_path == DB_FILE)
                .expect("database root is present in reset plan")
                .retired_relative_path,
        );
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::rename(&source, &destination).unwrap();

        assert!(apply_pending_v3_dataset_reset(data.path(), data.path()).unwrap());
        assert!(data.path().join("storage-generation").exists());
        assert!(has_phase(data.path(), "generation-installed"));
    }

    #[test]
    fn fresh_empty_root_does_not_arm_reset() {
        let data = tempfile::tempdir().unwrap();
        assert_eq!(
            prepare_v3_dataset(data.path(), data.path()).unwrap(),
            DatasetPreparation::Unchanged
        );
        assert!(!data.path().join(V3_DATASET_RESET_DIR).exists());
    }

    #[test]
    fn database_without_receipt_waits_for_application_probe() {
        let data = tempfile::tempdir().unwrap();
        touch(&data.path().join(DB_FILE));
        fs::create_dir_all(data.path().join("conversations")).unwrap();
        touch(&data.path().join("conversations").join("v3.txt"));

        assert_eq!(
            prepare_v3_dataset(data.path(), data.path()).unwrap(),
            DatasetPreparation::Unchanged
        );
        assert!(data.path().join(DB_FILE).is_file());
        assert!(data.path().join("conversations/v3.txt").is_file());
        assert!(!data.path().join(V3_DATASET_RESET_DIR).exists());
    }

    #[test]
    fn receipt_is_bound_to_canonical_resolved_work_root() {
        let data = tempfile::tempdir().unwrap();
        let first_work = tempfile::tempdir().unwrap();
        let second_work = tempfile::tempdir().unwrap();
        let generation = Uuid::now_v7().to_string();
        touch(&data.path().join(DB_FILE));
        fs::write(
            data.path().join(STORAGE_GENERATION_FILE),
            generation.as_bytes(),
        )
        .unwrap();
        write_v3_dataset_receipt_for_work_dir(
            data.path(),
            first_work.path(),
            &generation,
        )
        .unwrap();

        assert_eq!(
            inspect_v3_dataset_receipt(data.path(), first_work.path()).unwrap(),
            DatasetReceiptStatus::Current
        );
        assert_eq!(
            inspect_v3_dataset_receipt(data.path(), second_work.path()).unwrap(),
            DatasetReceiptStatus::WorkRootMismatch
        );
        assert!(
            require_current_v3_dataset_for_work_dir(data.path(), second_work.path()).is_err()
        );
        require_current_v3_dataset(data.path()).unwrap();
    }

    #[test]
    fn unfinished_bootstrap_binding_recovers_only_with_the_same_work_root() {
        let data = tempfile::tempdir().unwrap();
        let first_work = tempfile::tempdir().unwrap();
        let second_work = tempfile::tempdir().unwrap();
        let generation = Uuid::now_v7().to_string();
        fs::write(
            data.path().join(STORAGE_GENERATION_FILE),
            generation.as_bytes(),
        )
        .unwrap();
        write_v3_dataset_bootstrap_binding(data.path(), first_work.path(), &generation)
            .unwrap();

        assert_eq!(
            inspect_v3_dataset_bootstrap_binding(data.path(), first_work.path()).unwrap(),
            DatasetReceiptStatus::Current
        );
        assert_eq!(
            prepare_v3_dataset(data.path(), first_work.path()).unwrap(),
            DatasetPreparation::Unchanged
        );
        assert!(
            prepare_v3_dataset(data.path(), second_work.path())
                .unwrap_err()
                .to_string()
                .contains("different resolved work root")
        );
        assert!(!data.path().join(V3_DATASET_RESET_DIR).exists());
    }

    #[test]
    fn final_receipt_replaces_unfinished_bootstrap_binding() {
        let data = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let generation = Uuid::now_v7().to_string();
        touch(&data.path().join(DB_FILE));
        fs::write(
            data.path().join(STORAGE_GENERATION_FILE),
            generation.as_bytes(),
        )
        .unwrap();
        write_v3_dataset_bootstrap_binding(data.path(), work.path(), &generation).unwrap();
        assert!(data.path().join(V3_DATASET_BOOTSTRAP_FILE).is_file());

        write_v3_dataset_receipt_for_work_dir(data.path(), work.path(), &generation).unwrap();

        assert!(!data.path().join(V3_DATASET_BOOTSTRAP_FILE).exists());
        assert_eq!(
            inspect_v3_dataset_receipt(data.path(), work.path()).unwrap(),
            DatasetReceiptStatus::Current
        );
    }

    #[test]
    fn finalization_requires_matching_receipt_and_fresh_database() {
        let data = tempfile::tempdir().unwrap();
        touch(&data.path().join(DB_FILE));
        request_v3_dataset_reset(data.path()).unwrap();
        assert_eq!(
            prepare_v3_dataset(data.path(), data.path()).unwrap(),
            DatasetPreparation::ResetApplied
        );
        let plan = read_pending_v3_reset(data.path(), data.path())
            .unwrap()
            .expect("pending reset plan");

        let missing_receipt = finalize_v3_dataset_reset(data.path(), data.path())
            .expect_err("receipt is mandatory");
        assert!(missing_receipt.to_string().contains("receipt"));

        touch(&data.path().join(DB_FILE));
        let wrong_generation = Uuid::now_v7().to_string();
        write_v3_dataset_receipt(data.path(), &wrong_generation).unwrap();
        let mismatched = finalize_v3_dataset_reset(data.path(), data.path())
            .expect_err("receipt generation must match the reset plan");
        assert!(mismatched.to_string().contains("does not match"));
        assert!(data.path().join(V3_DATASET_RESET_DIR).is_dir());

        write_v3_dataset_receipt(data.path(), &plan.generation).unwrap();
        assert!(finalize_v3_dataset_reset(data.path(), data.path()).unwrap());
        assert!(!data.path().join(V3_DATASET_RESET_DIR).exists());
        assert_eq!(
            inspect_v3_dataset_receipt(data.path(), data.path()).unwrap(),
            DatasetReceiptStatus::Current
        );
    }

    #[test]
    fn empty_data_dir_with_external_managed_workspace_is_retired() {
        let data = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let conversations = work.path().join(MANAGED_WORKSPACES_DIR);
        fs::create_dir_all(&conversations).unwrap();
        touch(&conversations.join("legacy.txt"));

        assert_eq!(
            prepare_v3_dataset(data.path(), work.path()).unwrap(),
            DatasetPreparation::ResetApplied
        );
        assert!(!conversations.exists());

        let plan = read_pending_v3_reset(data.path(), work.path())
            .unwrap()
            .expect("external workspace retirement must leave a pending reset plan");
        assert_eq!(plan.reason, DatasetResetReason::NonV3Dataset);
        assert!(
            work.path()
                .join(plan.work_retired_dir)
                .join(MANAGED_WORKSPACES_DIR)
                .join("legacy.txt")
                .is_file()
        );
    }

    #[test]
    fn database_probe_can_override_a_matching_but_forged_receipt() {
        let data = tempfile::tempdir().unwrap();
        let generation = Uuid::now_v7().to_string();
        touch(&data.path().join(DB_FILE));
        fs::write(
            data.path().join(STORAGE_GENERATION_FILE),
            generation.as_bytes(),
        )
        .unwrap();
        write_v3_dataset_receipt(data.path(), &generation).unwrap();

        assert_eq!(
            prepare_v3_dataset(data.path(), data.path()).unwrap(),
            DatasetPreparation::Unchanged,
            "the filesystem hand-off alone cannot inspect SQLite identity"
        );
        assert_eq!(
            retire_non_v3_dataset_after_probe(data.path(), data.path()).unwrap(),
            DatasetPreparation::ResetApplied
        );
        assert!(!data.path().join(DB_FILE).exists());

        let plan = read_pending_v3_reset(data.path(), data.path())
            .unwrap()
            .expect("probe-triggered retirement must leave a pending reset plan");
        assert_eq!(plan.reason, DatasetResetReason::NonV3Dataset);
        assert!(
            data.path()
                .join(plan.retired_dir)
                .join(DB_FILE)
                .is_file()
        );
    }

    #[test]
    fn offline_dataset_gate_rejects_explicit_reset_request() {
        let data = tempfile::tempdir().unwrap();
        let generation = Uuid::now_v7().to_string();
        touch(&data.path().join(DB_FILE));
        fs::write(
            data.path().join(STORAGE_GENERATION_FILE),
            generation.as_bytes(),
        )
        .unwrap();
        write_v3_dataset_receipt(data.path(), &generation).unwrap();
        require_current_v3_dataset(data.path()).unwrap();

        request_v3_dataset_reset(data.path()).unwrap();
        let error = require_current_v3_dataset(data.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("explicit v3 dataset reset has been requested")
        );
    }
}

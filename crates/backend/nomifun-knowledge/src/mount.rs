//! Platform-aware mount engine: materializes knowledge bases inside a
//! workspace at `.nomi/knowledge/{link_name}` using NTFS junctions on
//! Windows (no privilege required), symlinks on Unix, and a recursive copy
//! as last-resort fallback (same degradation strategy as the skill linker in
//! `nomifun-extension`).
//!
//! The mount directory is wholly owned by this module: anything inside it
//! that is not in the desired set (or in [`MANAGED_KEEP`]) gets removed on
//! the next sync. Targets are never touched — removal only deletes the link
//! (or the fallback copy). Sibling `.nomi/` trees (`.nomi/skills`, …) are
//! never touched either.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::KB_MOUNT_REL_DIR;

/// One desired mount: `{workspace}/.nomi/knowledge/{link_name}` → `target`.
#[derive(Debug, Clone)]
pub struct MountSpec {
    pub link_name: String,
    pub target: PathBuf,
}

/// Platform-managed companion files inside the mount root (the self-ignore,
/// the terminal-facing README) — exempt from the stale-entry sweep, and
/// reserved against base link names (see `service::unique_link_name`).
pub(crate) const MANAGED_KEEP: &[&str] = &[".gitignore", "README.md"];

/// Synchronize the workspace mount directory to exactly `specs`.
///
/// Returns the link names that are present (linked or copied) after the
/// sync. Individual failures are logged and skipped — mounting must never
/// brick a session start.
pub async fn sync_mounts(workspace: &Path, specs: Vec<MountSpec>) -> Vec<String> {
    let workspace = workspace.to_path_buf();
    tokio::task::spawn_blocking(move || sync_mounts_blocking(&workspace, &specs))
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "knowledge mount task join error");
            Vec::new()
        })
}

fn sync_mounts_blocking(workspace: &Path, specs: &[MountSpec]) -> Vec<String> {
    sync_mounts_inner(workspace, specs)
}

fn sync_mounts_inner(workspace: &Path, specs: &[MountSpec]) -> Vec<String> {
    let mount_root = workspace.join(KB_MOUNT_REL_DIR);

    if specs.is_empty() {
        // Nothing should be mounted: clear our directory if it exists, then
        // try to remove the (now empty) scaffolding. The parent `.nomi/` is
        // only removed when empty — sibling trees keep it alive. Errors are
        // non-fatal.
        if mount_root.exists() {
            if let Ok(entries) = std::fs::read_dir(&mount_root) {
                for entry in entries.flatten() {
                    remove_mount_entry(&entry.path());
                }
            }
            let _ = std::fs::remove_dir(&mount_root);
            if let Some(parent) = mount_root.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
        return Vec::new();
    }

    if let Err(e) = std::fs::create_dir_all(&mount_root) {
        tracing::warn!(path = %mount_root.display(), error = %e, "failed to create knowledge mount dir");
        return Vec::new();
    }

    // Self-ignore the mount directory: when the workspace is a user git
    // repo, junctions would otherwise expose the knowledge base content as
    // committable project files. The ignore file lives INSIDE
    // `.nomi/knowledge/` — never at the `.nomi/` root — so committable
    // siblings like `.nomi/skills` stay visible to git.
    let gitignore = mount_root.join(".gitignore");
    if !gitignore.exists() {
        if let Err(e) = std::fs::write(&gitignore, "*\n") {
            tracing::warn!(path = %gitignore.display(), error = %e, "failed to write knowledge mount .gitignore");
        }
    }

    let desired: HashMap<&str, &MountSpec> = specs.iter().map(|s| (s.link_name.as_str(), s)).collect();

    // Pass 1: drop stale entries and stale links whose target changed.
    if let Ok(entries) = std::fs::read_dir(&mount_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if MANAGED_KEEP.contains(&name.as_str()) {
                continue;
            }
            match desired.get(name.as_str()) {
                None => remove_mount_entry(&path),
                Some(spec) => {
                    if let Some(current) = read_link_target(&path)
                        && current != spec.target
                    {
                        remove_mount_entry(&path);
                    }
                    // A non-link entry (copy fallback) is left in place: we
                    // cannot cheaply verify it, and re-copying every session
                    // start would be wasteful. It gets refreshed whenever the
                    // base set changes its name (different link_name).
                }
            }
        }
    }

    // Pass 2: create whatever is missing.
    let mut present = Vec::new();
    for spec in specs {
        let link = mount_root.join(&spec.link_name);
        if link.exists() || read_link_target(&link).is_some() {
            present.push(spec.link_name.clone());
            continue;
        }
        if !spec.target.is_dir() {
            tracing::warn!(
                target = %spec.target.display(),
                name = %spec.link_name,
                "knowledge base root missing; skipping mount"
            );
            continue;
        }
        match create_link(&spec.target, &link) {
            Ok(()) => present.push(spec.link_name.clone()),
            Err(e) => {
                tracing::warn!(
                    target = %spec.target.display(),
                    link = %link.display(),
                    error = %e,
                    raw_os_error = ?e.raw_os_error(),
                    "knowledge link failed; falling back to copy"
                );
                match copy_dir_recursive(&spec.target, &link) {
                    Ok(()) => present.push(spec.link_name.clone()),
                    Err(e) => {
                        tracing::warn!(link = %link.display(), error = %e, "knowledge copy fallback failed");
                    }
                }
            }
        }
    }
    present
}

/// Remove one entry inside the mount dir without ever touching the link
/// target's contents: junctions/symlinks are removed as links; plain
/// directories (copy fallback leftovers) are removed recursively — they are
/// copies we created, never user originals.
fn remove_mount_entry(path: &Path) {
    let result = if read_link_target(path).is_some() {
        remove_link_entry(path)
    } else if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    if let Err(e) = result {
        tracing::warn!(path = %path.display(), error = %e, "failed to remove stale knowledge mount entry");
    }
}

#[cfg(unix)]
fn remove_link_entry(path: &Path) -> io::Result<()> {
    std::fs::remove_file(path)
}

#[cfg(windows)]
fn remove_link_entry(path: &Path) -> io::Result<()> {
    if path.is_dir() {
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

fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in walkdir::WalkDir::new(src).min_depth(1) {
        let entry = entry.map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let rel = entry
            .path()
            .strip_prefix(src)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let to = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&to)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_base(dir: &TempDir, name: &str) -> PathBuf {
        let root = dir.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("note.md"), "# hi").unwrap();
        root
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
        // Skip the assertion when the platform degraded to a copy (no link
        // semantics) — detectable because read_link_target returns None.
        if read_link_target(&mounted).is_some() {
            std::fs::write(mounted.join("written.md"), "wb").unwrap();
            assert!(kb.join("written.md").exists(), "write-back must land in the base root");
        }
    }
}

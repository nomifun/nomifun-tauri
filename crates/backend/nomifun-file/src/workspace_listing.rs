//! Single-level, workspace-scoped directory listing shared by the
//! conversation workspace rail (`GET /api/conversations/{id}/workspace`) and
//! the terminal workspace rail (`GET /api/terminals/{id}/workspace`).
//!
//! The caller resolves the workspace root (a conversation's
//! `extra.workspace`, a terminal's cwd, …); this function takes that root plus
//! a relative path and enumerates exactly one directory level under it,
//! enforcing workspace isolation:
//!
//! - reject `..` parent-traversal components in the relative path;
//! - canonicalize and require the browsed path to stay inside the root, with
//!   an allowance for symlinked sub-directories mounted inside the workspace
//!   (e.g. native skill dirs that point at the builtin skills corpus under the
//!   data-dir);
//! - cap relative depth at [`MAX_DIR_DEPTH`];
//! - optional case-insensitive name `search` filter.
//!
//! Entries are returned directories-first, then case-insensitively
//! alphabetical.

use std::path::{Component, Path};

use nomifun_api_types::WorkspaceEntry;
use nomifun_common::AppError;

/// Maximum relative directory depth that may be browsed under a workspace
/// root. Guards against unbounded recursion when a client walks a deep tree.
pub const MAX_DIR_DEPTH: usize = 10;

/// Enumerate a single directory level under `base`, scoped to `rel`.
///
/// `base` is the (already-resolved) workspace root. `rel` is the
/// workspace-relative path to list (`""` or `"/"` lists the root itself).
/// `search`, when set and non-empty, filters entries to names that contain it
/// case-insensitively.
///
/// Returns the directory's entries (directories first, then case-insensitive
/// alphabetical) or an [`AppError`] describing the isolation/IO failure.
pub fn list_workspace_level(
    base: &Path,
    rel: &str,
    search: Option<&str>,
) -> Result<Vec<WorkspaceEntry>, AppError> {
    let relative_path = rel.trim_start_matches('/');
    let relative_path_obj = Path::new(relative_path);
    if relative_path_obj
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(AppError::BadRequest(
            "Path traversal outside workspace is not allowed".into(),
        ));
    }

    // Resolve the browsed path relative to the workspace root.
    let browse_path = if relative_path.is_empty() {
        base.to_path_buf()
    } else {
        base.join(relative_path_obj)
    };

    // Security: reject direct traversal outside the workspace root, but allow
    // symlinked directories mounted inside the workspace (e.g. native skill
    // dirs that point at the builtin skills corpus under data-dir).
    //
    // A workspace root that does not exist (e.g. a hung AutoWork task whose
    // workspace was never materialized, or a torn-down temp workspace) is a
    // NotFound (404), NOT an internal server error — otherwise the workspace
    // rail re-polls and every poll logs a spurious 500 (see the crash-report
    // triage: the conversation-#2 "500 storm" was this exact misc: a missing
    // root canonicalize failure surfacing as 500 instead of 404).
    let canonical_base = base.canonicalize().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AppError::NotFound("Workspace directory not found".into())
        } else {
            AppError::Internal(format!("Failed to resolve workspace path: {e}"))
        }
    })?;
    let canonical_browse = browse_path
        .canonicalize()
        .map_err(|_| AppError::NotFound("Directory not found".into()))?;
    if !browse_path.starts_with(base) && !canonical_browse.starts_with(&canonical_base) {
        return Err(AppError::BadRequest(
            "Path traversal outside workspace is not allowed".into(),
        ));
    }

    // Check depth limit.
    let depth = relative_path_obj.components().count();
    if depth > MAX_DIR_DEPTH {
        return Err(AppError::BadRequest(format!(
            "Directory depth exceeds maximum of {MAX_DIR_DEPTH}"
        )));
    }

    let search_lower = search
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase());

    let mut entries = Vec::new();
    let dir_reader = std::fs::read_dir(&canonical_browse)
        .map_err(|e| AppError::Internal(format!("Failed to read directory: {e}")))?;

    for entry in dir_reader {
        // A single unreadable directory entry must not sink the whole listing:
        // skip-and-log, mirroring the per-item resilience the conversation list
        // already uses. Hard-failing here is what turned one bad entry (e.g. a
        // dangling symlink from a hung installer) into a persistent 500 storm.
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!(
                    dir = %canonical_browse.display(),
                    error = %e,
                    "workspace listing: skipping unreadable directory entry"
                );
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();

        // Apply search filter if provided.
        if let Some(ref needle) = search_lower
            && !name.to_lowercase().contains(needle)
        {
            continue;
        }

        // Classify the entry. Prefer `metadata` (which FOLLOWS symlinks) so a
        // symlinked sub-directory mounted inside the workspace (native skill
        // dirs) is still reported as a directory. On error — the common case
        // being a dangling symlink whose target is missing — fall back to
        // `symlink_metadata` so the entry is still listed rather than failing
        // the request. Only if even the lstat fails do we skip-and-log it.
        let is_dir = match std::fs::metadata(entry.path()) {
            Ok(md) => md.is_dir(),
            Err(follow_err) => match std::fs::symlink_metadata(entry.path()) {
                Ok(md) => md.is_dir(),
                Err(stat_err) => {
                    tracing::warn!(
                        entry = %name,
                        follow_error = %follow_err,
                        stat_error = %stat_err,
                        "workspace listing: skipping unstattable entry"
                    );
                    continue;
                }
            },
        };

        let entry_type = if is_dir { "directory" } else { "file" };

        entries.push(WorkspaceEntry {
            name,
            entry_type: entry_type.into(),
        });
    }

    // Sort: directories first, then alphabetically (case-insensitive).
    entries.sort_by(|a, b| {
        let type_cmp = a.entry_type.cmp(&b.entry_type);
        if type_cmp == std::cmp::Ordering::Equal {
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        } else {
            type_cmp
        }
    });

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn lists_one_level_with_type() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        let mut out = list_workspace_level(dir.path(), "", None).unwrap();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "a.txt");
        assert_eq!(out[0].entry_type, "file");
        assert_eq!(out[1].name, "sub");
        assert_eq!(out[1].entry_type, "directory");
    }

    #[test]
    fn rejects_parent_traversal() {
        let dir = tempdir().unwrap();
        let err = list_workspace_level(dir.path(), "../", None);
        assert!(err.is_err(), "`..` must be rejected");
    }

    #[test]
    fn search_filters_case_insensitive() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "x").unwrap();
        fs::write(dir.path().join("readme.md"), "x").unwrap();
        let out = list_workspace_level(dir.path(), "", Some("cargo")).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "Cargo.toml");
    }

    #[test]
    fn missing_workspace_root_is_not_found_not_internal() {
        // A conversation whose workspace dir was never materialized (e.g. a hung
        // AutoWork install task) must not 500 on every poll — a missing root is
        // a 404, not an internal server error.
        let dir = tempdir().unwrap();
        let missing = dir.path().join("never-created");
        let err = list_workspace_level(&missing, "", None).unwrap_err();
        assert!(
            matches!(err, AppError::NotFound(_)),
            "missing workspace root must map to NotFound (404), got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_entry_does_not_fail_the_whole_listing() {
        // A hung installer readily leaves a dangling symlink (target never
        // downloaded). One unstattable entry must NOT hard-fail the request —
        // it is listed (as a plain entry) and the good entries still return.
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("good.txt"), "x").unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("nonexistent-target"),
            dir.path().join("broken-link"),
        )
        .unwrap();

        let out = list_workspace_level(dir.path(), "", None).expect("a dangling symlink must not 500 the listing");
        let names: Vec<&str> = out.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"good.txt"), "good entries must still be returned: {names:?}");
        assert!(
            names.contains(&"broken-link"),
            "the dangling symlink itself should still be listed, not drop the request: {names:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_subdir_stays_classified_as_directory() {
        // Regression guard: the workspace deliberately supports symlinked
        // sub-directories mounted inside it (native skill dirs -> builtin skills
        // corpus). Classification must still FOLLOW the link so such a dir stays
        // expandable — i.e. we must not switch wholesale to symlink_metadata.
        let dir = tempdir().unwrap();
        let real = dir.path().join("real-dir");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("link-dir")).unwrap();

        let out = list_workspace_level(dir.path(), "", None).unwrap();
        let link = out.iter().find(|e| e.name == "link-dir").expect("symlinked dir must be listed");
        assert_eq!(
            link.entry_type, "directory",
            "a symlinked sub-directory must remain classified as a directory"
        );
    }
}

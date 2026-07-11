//! Write-root containment guard (design §3.6 "写根包含校验").
//!
//! An **opt-in** guardrail: when a write root is configured, the file-mutating
//! tools (Write / Edit / ApplyPatch) refuse to write outside it. Default is no
//! root → no containment, so existing behaviour is byte-for-byte unchanged.
//!
//! # Threat model (honest scope)
//!
//! This stops *accidental or buggy* out-of-workspace writes (a bad absolute
//! path, a `../../` traversal, or a symlink that escapes the root). It is **not**
//! a security sandbox against a determined agent: the same agent has `Bash`, so
//! a real boundary needs OS-level confinement (macOS Seatbelt / Linux
//! namespaces), which is a separate, runtime-verified piece. Scoping it this way
//! avoids a false sense of safety.
//!
//! # Symlink correctness
//!
//! Containment is checked against the **canonicalised** path, not the textual
//! one: we resolve the longest existing ancestor (which collapses `..` and
//! follows symlinks) and re-append the not-yet-existing tail. A symlink inside
//! the root that points outside therefore resolves outside and is rejected —
//! textual `starts_with` alone would be fooled by it.

use std::path::{Path, PathBuf};

/// Resolve `path` for containment checking: canonicalise the longest existing
/// ancestor (resolving symlinks and `..`), then re-append the remaining
/// not-yet-existing components. Returns `None` if no ancestor exists or the
/// path has no components.
fn resolve_existing_prefix(path: &Path) -> Option<PathBuf> {
    // Fast path: the whole path exists (existing file or dir).
    if let Ok(c) = path.canonicalize() {
        return Some(c);
    }
    // Walk up to the nearest existing ancestor, canonicalise it, then re-attach
    // the trailing components that do not exist yet.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path;
    loop {
        let parent = cur.parent()?;
        tail.push(cur.file_name()?.to_os_string());
        if let Ok(c) = parent.canonicalize() {
            let mut resolved = c;
            for component in tail.iter().rev() {
                resolved.push(component);
            }
            return Some(resolved);
        }
        cur = parent;
    }
}

/// Whether `path` is contained within `root` after both are canonicalised.
/// A `root` that cannot be canonicalised (does not exist) yields `false` —
/// callers treat that as "cannot prove containment" → reject.
pub fn is_within_root(path: &Path, root: &Path) -> bool {
    let Ok(root_c) = root.canonicalize() else {
        return false;
    };
    match resolve_existing_prefix(path) {
        Some(target_c) => target_c.starts_with(&root_c),
        None => false,
    }
}

/// Resolve a model-supplied `file_path` for the file-mutating tools
/// (Write / Edit / ApplyPatch): a relative path is joined onto the session
/// working directory `cwd` (matching ReadTool / Grep / Glob / Bash); an
/// absolute path is returned unchanged. `cwd == None` leaves the path as-is, so
/// relative paths then resolve against the process cwd — the legacy behaviour.
///
/// This closes the read/write asymmetry: without it a relative path written by
/// the model lands against the Tauri process cwd rather than the conversation's
/// workspace, producing a truthful "Created …" while the file never appears in
/// the workspace the UI browses.
pub fn resolve_against_cwd(file_path: &str, cwd: Option<&Path>) -> String {
    match cwd {
        Some(cwd) if !Path::new(file_path).is_absolute() => {
            cwd.join(file_path).to_string_lossy().into_owned()
        }
        _ => file_path.to_owned(),
    }
}

/// Guard a write to `file_path` against an optional `root`. Returns `Some(error)`
/// when the write must be rejected, `None` when allowed (no root, or contained).
pub fn ensure_within_root(file_path: &str, root: Option<&Path>) -> Option<String> {
    let root = root?;
    if is_within_root(Path::new(file_path), root) {
        None
    } else {
        Some(format!(
            "Write rejected: {} is outside the allowed write root {}. \
             (Move the target inside the workspace, or disable tools.write_root.)",
            file_path,
            root.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn allows_existing_file_inside_root() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        fs::write(&f, "x").unwrap();
        assert!(is_within_root(&f, dir.path()));
    }

    #[test]
    fn allows_new_file_inside_root() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("sub/new.txt"); // sub/ may not exist yet
        // parent sub/ does not exist; resolve_existing_prefix walks to dir.
        assert!(is_within_root(&f, dir.path()));
    }

    #[test]
    fn rejects_absolute_path_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let f = other.path().join("escape.txt");
        assert!(!is_within_root(&f, dir.path()));
    }

    #[test]
    fn rejects_parent_traversal_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        // root/../sibling.txt resolves to dir/sibling.txt — outside root.
        let escape = root.join("../sibling.txt");
        assert!(!is_within_root(&escape, &root));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escaping_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        // root/link -> outside ; a write to root/link/file actually lands outside.
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let via_link = root.join("link/file.txt");
        assert!(
            !is_within_root(&via_link, &root),
            "a symlink escaping the root must be rejected (textual check would pass)"
        );
    }

    #[test]
    fn resolve_against_cwd_joins_relative_and_keeps_absolute() {
        // Use real absolute dirs so the test is platform-agnostic (a leading "/"
        // is NOT absolute on Windows, so hardcoded unix paths would be wrong here).
        let cwd = std::env::temp_dir();
        // Relative → joined onto cwd (expected computed with the same join so the
        // separator is platform-correct).
        assert_eq!(
            resolve_against_cwd("notes.txt", Some(&cwd)),
            cwd.join("notes.txt").to_string_lossy().into_owned()
        );
        assert_eq!(
            resolve_against_cwd("a/b.txt", Some(&cwd)),
            cwd.join("a/b.txt").to_string_lossy().into_owned()
        );
        // Absolute input → returned unchanged.
        let abs = cwd.join("already_absolute.txt");
        let abs_str = abs.to_str().unwrap();
        assert_eq!(resolve_against_cwd(abs_str, Some(&cwd)), abs_str);
        // No cwd → unchanged (legacy: relative resolves against the process cwd).
        assert_eq!(resolve_against_cwd("notes.txt", None), "notes.txt");
    }

    #[test]
    fn ensure_within_root_is_noop_without_a_root() {
        // No configured root → never rejects (default behaviour unchanged).
        assert!(ensure_within_root("/anywhere/at/all.txt", None).is_none());
    }

    #[test]
    fn ensure_within_root_rejects_outside() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let outside = other.path().join("x.txt");
        assert!(ensure_within_root(outside.to_str().unwrap(), Some(dir.path())).is_some());
        let inside = dir.path().join("x.txt");
        assert!(ensure_within_root(inside.to_str().unwrap(), Some(dir.path())).is_none());
    }
}

//! Filesystem + shell-open domain capabilities (registry form): file read/write,
//! directory browsing, metadata retrieval, entry removal/rename, and OS-level
//! open (URLs, files, apps).
//!
//! The file service operates within a sandbox defined by `allowed_roots` — all
//! paths are validated against these roots before any I/O. The gateway receives
//! a `FileServiceRef` (= `Arc<dyn IFileService>`) which already performs this
//! validation internally.
//!
//! The shell service wraps OS-native open commands with URL-scheme and path
//! validation (only http/https/mailto, existing paths).
//!
//! PATH SCOPING: The `IFileService` methods that take `extra_root: Option<&Path>`
//! allow callers to widen the sandbox per-request — we pass `None` here (strict
//! mode: only `allowed_roots` configured at construction time apply). Write
//! operations require a `workspace` parameter for event scoping.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::{CallerCtx, GatewayDeps};
use crate::registry::{Capability, CapabilityMeta, DangerTier, Surface};
use crate::server::ok;

use nomifun_file::PathAuthority;

// ── Constants ────────────────────────────────────────────────────────────────

/// Cap read_file output to avoid blowing up the LLM context.
const READ_FILE_MAX_BYTES: usize = 64 * 1024;

// ── Param structs (single source: schema + runtime) ─────────────────────────

/// Read a file as UTF-8 text.
#[derive(Deserialize, JsonSchema)]
struct ReadFileParams {
    /// Absolute path to the file to read.
    path: String,
}

/// Write (create or overwrite) a file with the given content.
#[derive(Deserialize, JsonSchema)]
struct WriteFileParams {
    /// Absolute path to the file to write.
    path: String,
    /// UTF-8 text content to write.
    content: String,
    /// Workspace root directory (used for event scoping). Must be an
    /// allowed root or ancestor of `path`.
    workspace: String,
}

/// List immediate children of a directory.
#[derive(Deserialize, JsonSchema)]
struct BrowseParams {
    /// Absolute path to the directory to list.
    dir: String,
    /// Workspace root used to compute relative paths in the response.
    root: String,
}

/// Recursively list all files under a workspace root as a flat list.
#[derive(Deserialize, JsonSchema)]
struct ListWorkspaceFilesParams {
    /// Absolute path to the workspace root.
    root: String,
}

/// Get metadata (name, size, mime, last_modified, is_directory) for a path.
#[derive(Deserialize, JsonSchema)]
struct GetMetadataParams {
    /// Absolute path to the file or directory.
    path: String,
}

/// Delete a file or directory (recursively).
#[derive(Deserialize, JsonSchema)]
struct RemoveParams {
    /// Absolute path to the file or directory to remove.
    path: String,
    /// Workspace root (used for event scoping).
    workspace: String,
}

/// Rename a file or directory (same parent, new name).
#[derive(Deserialize, JsonSchema)]
struct RenameParams {
    /// Absolute path to the file or directory to rename.
    path: String,
    /// New name (just the filename, not a full path).
    new_name: String,
}

/// Open a URL in the default browser (http/https/mailto only).
#[derive(Deserialize, JsonSchema)]
struct ShellOpenExternalParams {
    /// URL to open (must be http://, https://, or mailto:).
    url: String,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// Resolve the filesystem authority for a file operation from the caller's trust
/// surface. A trusted local **Desktop** session (the machine owner driving their
/// own agent) gets [`PathAuthority::Unrestricted`] — the OS user's own
/// permissions are the only boundary, so the agent can operate on any path the
/// user asks for (e.g. `C:\code\...` outside the sandbox roots). External
/// **Channel/Remote** sessions return `None`, keeping the file service's default
/// `allowed_roots` confinement (unchanged, fail-safe for untrusted strangers).
///
/// This governs only WHERE a file op may act. WHETHER it may run (the
/// destructive/sensitive confirmation gate) stays with the DangerTier matrix in
/// `registry::decide`, orthogonal to authority — a Desktop `remove` is still
/// confirm-gated even though it is unrestricted in path.
fn file_authority(ctx: &CallerCtx) -> Option<PathAuthority> {
    match ctx.surface() {
        Surface::Desktop => Some(PathAuthority::Unrestricted),
        Surface::Channel | Surface::Remote => None,
    }
}

async fn read_file(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: ReadFileParams) -> Value {
    let result = match file_authority(&ctx) {
        Some(auth) => deps.file_service.read_file_scoped(&p.path, &auth).await,
        None => deps.file_service.read_file(&p.path, None).await,
    };
    match result {
        Ok(Some(content)) => {
            if content.len() > READ_FILE_MAX_BYTES {
                let truncated = &content[..content.floor_char_boundary(READ_FILE_MAX_BYTES)];
                ok(json!({
                    "content": truncated,
                    "truncated": true,
                    "total_bytes": content.len(),
                    "note": format!("output capped at ~{}KB; file is {} bytes total", READ_FILE_MAX_BYTES / 1024, content.len()),
                }))
            } else {
                ok(json!({ "content": content, "truncated": false }))
            }
        }
        Ok(None) => json!({ "error": format!("file not found: {}", p.path) }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn write_file(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: WriteFileParams) -> Value {
    let result = match file_authority(&ctx) {
        Some(auth) => {
            deps.file_service
                .write_file_scoped(&p.path, p.content.as_bytes(), &p.workspace, &auth)
                .await
        }
        None => {
            deps.file_service
                .write_file(&p.path, p.content.as_bytes(), &p.workspace)
                .await
        }
    };
    match result {
        Ok(_) => ok(json!({ "written": true, "path": p.path })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn browse(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: BrowseParams) -> Value {
    let result = match file_authority(&ctx) {
        Some(auth) => deps.file_service.get_files_by_dir_scoped(&p.dir, &p.root, &auth).await,
        None => deps.file_service.get_files_by_dir(&p.dir, &p.root).await,
    };
    match result {
        Ok(entries) => {
            let items: Vec<Value> = entries
                .iter()
                .map(|e| {
                    json!({
                        "name": e.name,
                        "full_path": e.full_path,
                        "relative_path": e.relative_path,
                        "is_dir": e.is_dir,
                    })
                })
                .collect();
            ok(json!({ "entries": items, "count": items.len() }))
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn list_workspace_files(
    deps: Arc<GatewayDeps>,
    ctx: CallerCtx,
    p: ListWorkspaceFilesParams,
) -> Value {
    let result = match file_authority(&ctx) {
        Some(auth) => deps.file_service.list_workspace_files_scoped(&p.root, &auth).await,
        None => deps.file_service.list_workspace_files(&p.root).await,
    };
    match result {
        Ok(files) => {
            let items: Vec<Value> = files
                .iter()
                .map(|f| {
                    json!({
                        "name": f.name,
                        "full_path": f.full_path,
                        "relative_path": f.relative_path,
                    })
                })
                .collect();
            ok(json!({ "files": items, "count": items.len() }))
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn get_metadata(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: GetMetadataParams) -> Value {
    let result = match file_authority(&ctx) {
        Some(auth) => deps.file_service.get_file_metadata_scoped(&p.path, &auth).await,
        None => deps.file_service.get_file_metadata(&p.path, None).await,
    };
    match result {
        Ok(meta) => ok(json!({
            "name": meta.name,
            "path": meta.path,
            "size": meta.size,
            "mime_type": meta.mime_type,
            "last_modified": meta.last_modified,
            "is_directory": meta.is_directory,
        })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn remove(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: RemoveParams) -> Value {
    let result = match file_authority(&ctx) {
        Some(auth) => deps.file_service.remove_entry_scoped(&p.path, &p.workspace, &auth).await,
        None => deps.file_service.remove_entry(&p.path, &p.workspace).await,
    };
    match result {
        Ok(()) => ok(json!({ "removed": true, "path": p.path })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn rename(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: RenameParams) -> Value {
    let result = match file_authority(&ctx) {
        Some(auth) => deps.file_service.rename_entry_scoped(&p.path, &p.new_name, &auth).await,
        None => deps.file_service.rename_entry(&p.path, &p.new_name).await,
    };
    match result {
        Ok(new_path) => ok(json!({ "renamed": true, "new_path": new_path })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn shell_open_external(
    deps: Arc<GatewayDeps>,
    _ctx: CallerCtx,
    p: ShellOpenExternalParams,
) -> Value {
    match deps.shell_service.open_external(&p.url).await {
        Ok(()) => ok(json!({ "opened": true, "url": p.url })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── Registration ─────────────────────────────────────────────────────────────

/// Register the filesystem + shell-open domain capabilities.
pub(crate) fn register(out: &mut Vec<Capability>) {
    // 1. Read file (Read)
    out.push(Capability::new::<ReadFileParams, _, _>(
        CapabilityMeta::new(
            "nomi_fs_read_file",
            "files",
            "Read a file as UTF-8 text (output capped at ~64KB). Returns the content or an error if the path is outside the sandbox or does not exist.",
            DangerTier::Read,
        ),
        |deps, ctx, p| read_file(deps, ctx, p),
    ));

    // 2. Write file (Write, deny_on Channel)
    out.push(Capability::new::<WriteFileParams, _, _>(
        CapabilityMeta::new(
            "nomi_fs_write_file",
            "files",
            "Write (create or overwrite) a file with the given UTF-8 content. On a local desktop session this can target any path the OS user can write; on external channel/remote sessions it is confined to the session's allowed roots.",
            DangerTier::Write,
        )
        .deny_on(&[Surface::Channel]),
        |deps, ctx, p| write_file(deps, ctx, p),
    ));

    // 3. Browse directory (Read)
    out.push(Capability::new::<BrowseParams, _, _>(
        CapabilityMeta::new(
            "nomi_fs_browse",
            "files",
            "List immediate children of a directory (one level). Returns name, full_path, relative_path, and is_dir for each entry.",
            DangerTier::Read,
        ),
        |deps, ctx, p| browse(deps, ctx, p),
    ));

    // 4. List workspace files (Read)
    out.push(Capability::new::<ListWorkspaceFilesParams, _, _>(
        CapabilityMeta::new(
            "nomi_fs_list_workspace_files",
            "files",
            "Recursively list all files under a workspace root as a flat list (up to 20,000 entries). Useful for discovering project structure.",
            DangerTier::Read,
        ),
        |deps, ctx, p| list_workspace_files(deps, ctx, p),
    ));

    // 5. Get metadata (Read)
    out.push(Capability::new::<GetMetadataParams, _, _>(
        CapabilityMeta::new(
            "nomi_fs_get_metadata",
            "files",
            "Get metadata for a file or directory: name, path, size (bytes), MIME type, last_modified (unix timestamp), and whether it is a directory.",
            DangerTier::Read,
        ),
        |deps, ctx, p| get_metadata(deps, ctx, p),
    ));

    // 6. Remove entry (Destructive, deny_on Channel)
    out.push(Capability::new::<RemoveParams, _, _>(
        CapabilityMeta::new(
            "nomi_fs_remove",
            "files",
            "Delete a file or directory (recursively). Irreversible — the snapshot system can restore if initialized, but the raw FS deletion cannot be undone.",
            DangerTier::Destructive,
        )
        .deny_on(&[Surface::Channel]),
        |deps, ctx, p| remove(deps, ctx, p),
    ));

    // 7. Rename entry (Write, deny_on Channel)
    out.push(Capability::new::<RenameParams, _, _>(
        CapabilityMeta::new(
            "nomi_fs_rename",
            "files",
            "Rename a file or directory (same parent, new name). Returns the new absolute path on success.",
            DangerTier::Write,
        )
        .deny_on(&[Surface::Channel]),
        |deps, ctx, p| rename(deps, ctx, p),
    ));

    // 8. Shell open external (Write, deny_on Channel)
    out.push(Capability::new::<ShellOpenExternalParams, _, _>(
        CapabilityMeta::new(
            "nomi_shell_open_external",
            "files",
            "Open a URL in the default browser or mail client. Only http://, https://, and mailto: schemes are allowed.",
            DangerTier::Write,
        )
        .deny_on(&[Surface::Channel]),
        |deps, ctx, p| shell_open_external(deps, ctx, p),
    ));
}

// ── SKIPPED tools ────────────────────────────────────────────────────────────
//
// The following candidate tools were evaluated but NOT registered:
//
// - `nomi_fs_copy_files_to_workspace` — `copy_files_to_workspace` requires a
//   `source_root` and multiple file paths; more of a bulk UI operation than an
//   agent tool. Can be added later if needed.
//
// - `nomi_fs_create_temp_file` / `nomi_fs_create_upload_file` — temp/upload
//   helpers used by the UI upload flow and conversation attachments. The agent
//   can use `nomi_fs_write_file` for workspace files directly.
//
// - `nomi_fs_get_image_base64` / `nomi_fs_fetch_remote_image` — image
//   processing helpers. Could be added as a separate "media" domain if agents
//   need inline image data.
//
// - `nomi_fs_create_zip` / `nomi_fs_cancel_zip` — ZIP packaging operations.
//   Not a typical agent primitive; can be added if agent workflows need
//   archiving.
//
// - `nomi_shell_open_file` — `ShellService::open_file` opens a local file with
//   its default application. Could be added but overlaps with `open_external`
//   for most use cases and has less clear agent utility.
//
// - `nomi_shell_show_in_folder` — `ShellService::show_item_in_folder` reveals
//   an item in Finder/Explorer. Pure UI convenience; low agent utility.
//
// - `nomi_shell_launch` — `ShellService::launch` opens arbitrary targets
//   (apps, URLs, files) via the OS. Too broad for unsupervised agent use
//   (accepts any target string). If needed, can be added with Destructive tier.
//
// - `nomi_shell_open_folder_with` — `ShellService::open_folder_with` opens a
//   folder in VSCode/Terminal/Explorer. Agent-facing utility is limited; prefer
//   explicit terminal session creation via the terminal domain.
//
// - `nomi_shell_check_tool_installed` — `ShellService::check_tool_installed`
//   checks if VSCode/Terminal/Explorer is available. Informational but narrow.
//
// FileWatchService and SnapshotService methods are NOT included — they are
// session-lifecycle services (start/stop watch, git-style staging) that belong
// to the UI interaction layer, not agent tool primitives.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_authority_unrestricted_for_desktop_only() {
        // Trusted local desktop owner → OS-user authority.
        let desktop = CallerCtx::default();
        assert!(
            matches!(file_authority(&desktop), Some(PathAuthority::Unrestricted)),
            "desktop must get Unrestricted"
        );
        // External IM channel stranger → keep the default allowed_roots confinement.
        let channel = CallerCtx { channel_platform: Some("lark".into()), ..Default::default() };
        assert!(file_authority(&channel).is_none(), "channel must not be unrestricted");
        // Remote front-door consumer → likewise confined.
        let remote = CallerCtx { remote: true, ..Default::default() };
        assert!(file_authority(&remote).is_none(), "remote must not be unrestricted");
    }
}

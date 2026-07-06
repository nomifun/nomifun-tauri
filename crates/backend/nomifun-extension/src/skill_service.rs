use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use include_dir::{Dir, include_dir};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::constants::{
    ASSISTANT_RULES_DIR_NAME, ASSISTANT_SKILLS_DIR_NAME, BUILTIN_AUTO_SKILLS_SUBDIR,
    BUILTIN_RULES_DIR_NAME, COMMON_SKILL_DIRS, CRON_SKILLS_DIR_NAME, SKILL_MANIFEST_FILE,
    SKILLS_DIR_NAME,
};
use crate::error::ExtensionError;

/// Built-in skill corpus embedded into the binary at compile time.
///
/// Mirrors the strategy used by `nomifun-assistant::builtin`: the corpus is
/// authoritative at build time; an optional on-disk override
/// (`NOMIFUN_BUILTIN_SKILLS_PATH`) is consulted at runtime for rapid
/// iteration and E2E fixtures.
static BUILTIN_SKILLS: Dir<'static> =
    include_dir!("$CARGO_MANIFEST_DIR/../nomifun-app/assets/builtin-skills");

/// Name of the environment variable that, when set, overrides the embedded
/// corpus with an on-disk directory. Consumed by
/// [`resolve_skill_paths`] when building [`SkillPaths`].
pub const BUILTIN_SKILLS_ENV_VAR: &str = "NOMIFUN_BUILTIN_SKILLS_PATH";

/// Expose the embedded builtin skills corpus for startup
/// materialization. Consumers outside this crate should not depend on
/// `include_dir` directly.
pub fn builtin_skills_corpus() -> &'static Dir<'static> {
    &BUILTIN_SKILLS
}

/// Build the on-disk materialization version for the embedded builtin skill
/// corpus. This deliberately includes a content fingerprint, not just the app
/// version, so asset-only changes refresh `{data_dir}/builtin-skills` even
/// when the crate version stays unchanged during development.
pub fn builtin_skills_materialize_version(app_version: &str) -> String {
    let fingerprint = builtin_skills_corpus_fingerprint();
    format!("{app_version}+skills.{}", &fingerprint[..12])
}

/// Deterministic SHA-256 fingerprint for the embedded builtin skill corpus.
pub fn builtin_skills_corpus_fingerprint() -> String {
    let mut files = Vec::new();
    collect_corpus_files(&BUILTIN_SKILLS, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (path, contents) in files {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(contents);
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn collect_corpus_files(dir: &'static Dir<'static>, out: &mut Vec<(String, &'static [u8])>) {
    for file in dir.files() {
        out.push((file.path().to_string_lossy().into_owned(), file.contents()));
    }
    for subdir in dir.dirs() {
        collect_corpus_files(subdir, out);
    }
}

// ---------------------------------------------------------------------------
// Skill paths resolution
// ---------------------------------------------------------------------------

/// Resolved base directories for skill and rule management.
///
/// `builtin_skills_dir` always points at a real on-disk directory.
/// In production it resolves to `{data_dir}/builtin-skills/`, populated
/// at startup by [`crate::startup_materialize::materialize_if_needed`].
/// In dev/test it can be redirected via [`BUILTIN_SKILLS_ENV_VAR`].
#[derive(Debug, Clone)]
pub struct SkillPaths {
    /// Root data directory (~/.nomifun/).
    pub data_dir: PathBuf,
    /// User-created skills directory (~/.nomifun/skills/).
    pub user_skills_dir: PathBuf,
    /// Per-job cron skills directory (~/.nomifun/cron/skills/).
    pub cron_skills_dir: PathBuf,
    /// Built-in skills directory on disk. Always set.
    /// Points to `{data_dir}/builtin-skills/` in production (populated at
    /// startup by `startup_materialize::materialize_if_needed`) or
    /// wherever [`BUILTIN_SKILLS_ENV_VAR`] points in dev mode.
    pub builtin_skills_dir: PathBuf,
    /// Built-in rules directory (app bundle resource).
    pub builtin_rules_dir: PathBuf,
    /// Assistant-level rules directory (~/.nomifun/assistant-rules/).
    pub assistant_rules_dir: PathBuf,
    /// Assistant-level skills directory (~/.nomifun/assistant-skills/).
    pub assistant_skills_dir: PathBuf,
}

/// Resolve standard skill paths.
///
/// `app_resource_dir` is the application's bundled resource directory
/// (e.g. the binary's parent or a configured resource path); only
/// `builtin_rules_dir` is still derived from it — built-in skills live
/// under `data_dir` (materialized at startup from the embedded corpus)
/// unless redirected via [`BUILTIN_SKILLS_ENV_VAR`].
///
/// `data_dir` is the user-level data root (e.g. `~/.nomifun/`) and
/// determines where user skills, assistant resources, and the built-in
/// skills tree (`{data_dir}/builtin-skills/`) live. Per-conversation
/// agent skills are no longer materialized on disk — see
/// [`materialize_skills_for_agent`] for the symlink contract.
pub fn resolve_skill_paths(app_resource_dir: &Path, data_dir: &Path) -> SkillPaths {
    let builtin_skills_dir = std::env::var(BUILTIN_SKILLS_ENV_VAR)
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join(crate::constants::BUILTIN_SKILLS_DIR_NAME));

    SkillPaths {
        data_dir: data_dir.to_path_buf(),
        user_skills_dir: data_dir.join(SKILLS_DIR_NAME),
        cron_skills_dir: data_dir.join(CRON_SKILLS_DIR_NAME),
        builtin_skills_dir,
        builtin_rules_dir: app_resource_dir.join(BUILTIN_RULES_DIR_NAME),
        assistant_rules_dir: data_dir.join(ASSISTANT_RULES_DIR_NAME),
        assistant_skills_dir: data_dir.join(ASSISTANT_SKILLS_DIR_NAME),
    }
}

// ---------------------------------------------------------------------------
// A'. 伙伴自进化技能：分层范围 + 路径助手 + 写原语
//
// 专属技能落 {user_skills_dir}/companion/{companion_id}/{name}/，共享技能落
// {user_skills_dir}/shared/{name}/，草稿落 {user_skills_dir}/_drafts/{companion_id}/{name}/。
// 正文以磁盘 SKILL.md 为事实源（companion store 的 companion_skills 表只存元数据）。
// ---------------------------------------------------------------------------

/// 技能归属范围：`Shared`（全员可用）或 `Companion(id)`（伙伴专属）。
#[derive(Debug, Clone)]
pub enum SkillScope {
    Shared,
    Companion(String),
}

/// `{user_skills_dir}/companion`
pub fn companion_skills_root(paths: &SkillPaths) -> PathBuf {
    paths.user_skills_dir.join("companion")
}

/// `{user_skills_dir}/shared`
pub fn shared_skills_root(paths: &SkillPaths) -> PathBuf {
    paths.user_skills_dir.join("shared")
}

/// `{user_skills_dir}/_drafts`
pub fn drafts_root(paths: &SkillPaths) -> PathBuf {
    paths.user_skills_dir.join("_drafts")
}

/// Resolve the on-disk directory for a scoped skill. `draft=true` routes to the
/// review staging area. Both `name` and any `companion_id` are validated against
/// path traversal via [`validate_filename`].
pub fn skill_dir_for(
    paths: &SkillPaths,
    scope: &SkillScope,
    name: &str,
    draft: bool,
) -> Result<PathBuf, ExtensionError> {
    validate_filename(name)?;
    let base = match (scope, draft) {
        (SkillScope::Companion(cid), false) => {
            validate_filename(cid)?;
            companion_skills_root(paths).join(cid)
        }
        (SkillScope::Companion(cid), true) => {
            validate_filename(cid)?;
            drafts_root(paths).join(cid)
        }
        (SkillScope::Shared, _) => shared_skills_root(paths),
    };
    Ok(base.join(name))
}

/// 起草一份技能所需的字段。`name`/`description` 必填，其余可选。
#[derive(Debug, Clone)]
pub struct SkillDraftInput {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub allowed_tools: Option<String>,
    pub paths: Option<String>,
    pub body: String,
}

/// 拼一份合法 SKILL.md：YAML frontmatter（name/description 必填，其余可选）+ 正文。
pub fn build_skill_md(input: &SkillDraftInput) -> String {
    let mut fm = String::from("---\n");
    fm.push_str(&format!("name: {}\n", input.name));
    fm.push_str(&format!("description: {}\n", input.description));
    if let Some(w) = &input.when_to_use {
        if !w.is_empty() {
            fm.push_str(&format!("when-to-use: {w}\n"));
        }
    }
    if let Some(t) = &input.allowed_tools {
        if !t.is_empty() {
            fm.push_str(&format!("allowed-tools: {t}\n"));
        }
    }
    if let Some(p) = &input.paths {
        if !p.is_empty() {
            fm.push_str(&format!("paths: {p}\n"));
        }
    }
    fm.push_str("---\n\n");
    fm.push_str(&input.body);
    if !input.body.ends_with('\n') {
        fm.push('\n');
    }
    fm
}

/// 把 agent 起草的字段物化成磁盘上的 SKILL.md（整条自进化链路唯一缺失的底层原语）。
/// 空 description 直接拒（frontmatter 双侧契约）；`draft=true` 落到审阅暂存区。
pub async fn create_skill(
    paths: &SkillPaths,
    scope: &SkillScope,
    draft: bool,
    input: &SkillDraftInput,
) -> Result<PathBuf, ExtensionError> {
    validate_filename(&input.name)?;
    if input.description.trim().is_empty() {
        return Err(ExtensionError::InvalidSkillPath(format!(
            "skill '{}' has empty description",
            input.name
        )));
    }
    let dir = skill_dir_for(paths, scope, &input.name, draft)?;
    tokio::fs::create_dir_all(&dir).await?;
    let content = build_skill_md(input);
    tokio::fs::write(dir.join(SKILL_MANIFEST_FILE), content).await?;
    debug!(skill = %input.name, dir = %dir.display(), draft, "companion skill created");
    Ok(dir)
}

/// 应用内编辑：整文覆写一份已存在技能的 SKILL.md，但写前校验 frontmatter 合法且 description 非空。
pub async fn write_skill(
    paths: &SkillPaths,
    scope: &SkillScope,
    draft: bool,
    name: &str,
    full_markdown: &str,
) -> Result<(), ExtensionError> {
    let (_n, desc) = parse_frontmatter_fields(full_markdown).ok_or_else(|| {
        ExtensionError::InvalidSkillPath(format!("invalid frontmatter for skill '{name}'"))
    })?;
    if desc.trim().is_empty() {
        return Err(ExtensionError::InvalidSkillPath(format!(
            "skill '{name}' has empty description"
        )));
    }
    let dir = skill_dir_for(paths, scope, name, draft)?;
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(dir.join(SKILL_MANIFEST_FILE), full_markdown).await?;
    Ok(())
}

/// Copy a skill's SKILL.md from one scope to another (skill transfer / 互教). Reads the active
/// source SKILL.md and writes it into the target's active dir (validated by `write_skill`).
pub async fn copy_skill(
    paths: &SkillPaths,
    from: &SkillScope,
    to: &SkillScope,
    name: &str,
) -> Result<(), ExtensionError> {
    let src = skill_dir_for(paths, from, name, false)?;
    let content = tokio::fs::read_to_string(src.join(SKILL_MANIFEST_FILE)).await?;
    write_skill(paths, to, false, name, &content).await
}

// ---------------------------------------------------------------------------
// A. Built-in resource reading
// ---------------------------------------------------------------------------

/// Read a built-in rule file by name.
///
/// Returns the file content as a string. Returns an empty string if the
/// file does not exist (graceful degradation per API spec).
pub async fn read_builtin_rule(
    paths: &SkillPaths,
    file_name: &str,
) -> Result<String, ExtensionError> {
    validate_filename(file_name)?;
    let file_path = paths.builtin_rules_dir.join(file_name);
    read_file_or_empty(&file_path).await
}

/// Read a built-in skill file by name.
///
/// `file_name` is a relative path inside the built-in skills corpus
/// (e.g. `"auto-inject/cron/SKILL.md"` or `"mermaid/SKILL.md"`). Returns
/// the file content as a string, or an empty string if the file does not
/// exist (preserves the legacy graceful-degradation contract consumed by
/// the renderer).
///
/// Reads from `paths.builtin_skills_dir`, which is always populated at
/// startup by [`crate::startup_materialize::materialize_if_needed`].
/// Rejects `..`-style traversal.
pub async fn read_builtin_skill(
    paths: &SkillPaths,
    file_name: &str,
) -> Result<String, ExtensionError> {
    validate_builtin_skill_path(file_name)?;
    let file_path = paths.builtin_skills_dir.join(file_name);
    read_file_or_empty(&file_path).await
}

// ---------------------------------------------------------------------------
// B. Assistant-level CRUD
// ---------------------------------------------------------------------------

/// Read an assistant rule with locale fallback.
///
/// Fallback order:
/// 1. `{assistantId}.{locale}.md` (if locale provided)
/// 2. `{assistantId}.md`
/// 3. Empty string
pub async fn read_assistant_rule(
    paths: &SkillPaths,
    assistant_id: &str,
    locale: Option<&str>,
) -> Result<String, ExtensionError> {
    read_assistant_resource(&paths.assistant_rules_dir, assistant_id, locale).await
}

/// Write an assistant rule.
///
/// Creates `{assistantId}.{locale}.md` or `{assistantId}.md` in the
/// assistant rules directory.
pub async fn write_assistant_rule(
    paths: &SkillPaths,
    assistant_id: &str,
    content: &str,
    locale: Option<&str>,
) -> Result<bool, ExtensionError> {
    write_assistant_resource(&paths.assistant_rules_dir, assistant_id, content, locale).await
}

/// Delete all locale versions of an assistant rule.
pub async fn delete_assistant_rule(
    paths: &SkillPaths,
    assistant_id: &str,
) -> Result<bool, ExtensionError> {
    delete_assistant_resource(&paths.assistant_rules_dir, assistant_id).await
}

/// Read an assistant skill with locale fallback.
pub async fn read_assistant_skill(
    paths: &SkillPaths,
    assistant_id: &str,
    locale: Option<&str>,
) -> Result<String, ExtensionError> {
    read_assistant_resource(&paths.assistant_skills_dir, assistant_id, locale).await
}

/// Write an assistant skill.
pub async fn write_assistant_skill(
    paths: &SkillPaths,
    assistant_id: &str,
    content: &str,
    locale: Option<&str>,
) -> Result<bool, ExtensionError> {
    write_assistant_resource(&paths.assistant_skills_dir, assistant_id, content, locale).await
}

/// Delete all locale versions of an assistant skill.
pub async fn delete_assistant_skill(
    paths: &SkillPaths,
    assistant_id: &str,
) -> Result<bool, ExtensionError> {
    delete_assistant_resource(&paths.assistant_skills_dir, assistant_id).await
}

// ---------------------------------------------------------------------------
// C. Skill listing & info
// ---------------------------------------------------------------------------

/// Origin of a listed skill.
///
/// Matches the renderer contract in
/// `src/common/adapter/ipcBridge.ts::listAvailableSkills`, which filters the
/// Skills Hub UI by this value. `Extension` is reserved for
/// extension-contributed skills once `ExtensionRegistry` is wired into the
/// Rust backend; the pilot only emits `Builtin` / `Custom`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    Builtin,
    Custom,
    Extension,
}

/// A discovered skill item for listing.
///
/// For `source=Builtin`, `location` is the absolute path of the on-disk
/// SKILL.md under `paths.builtin_skills_dir` (populated at startup by
/// [`crate::startup_materialize::materialize_if_needed`]). The
/// `relative_location` carries the relative path suitable for
/// `POST /api/skills/builtin-skill` (e.g. `"auto-inject/cron/SKILL.md"`
/// or `"mermaid/SKILL.md"`). Other sources leave `relative_location`
/// `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillListItem {
    pub name: String,
    pub description: String,
    pub location: String,
    pub relative_location: Option<String>,
    pub is_custom: bool,
    pub source: SkillSource,
}

/// List all available skills (built-in + user custom), deduplicated.
///
/// User custom skills override built-in skills with the same name.
///
/// For built-in entries, the caller sees an absolute `location` pointing
/// at `paths.builtin_skills_dir/.../SKILL.md` — the tree is populated
/// at startup by
/// [`crate::startup_materialize::materialize_if_needed`] so downstream
/// consumers (e.g. the SkillsHubSettings export-symlink flow) can
/// resolve the path on disk. `relative_location` is populated for
/// built-ins only.
pub async fn list_available_skills(
    paths: &SkillPaths,
) -> Result<Vec<SkillListItem>, ExtensionError> {
    let mut builtin_skills = std::collections::HashMap::new();

    // 1. Built-in skills (lower priority)
    for item in list_builtin_skills(paths).await {
        builtin_skills.insert(item.name.clone(), item);
    }

    // 2. User custom skills (higher priority, overrides builtin)
    let mut custom_skills = Vec::new();
    if let Ok(entries) = scan_skill_dirs(&paths.user_skills_dir).await {
        for item in entries {
            builtin_skills.remove(&item.name);
            custom_skills.push(SkillListItem {
                name: item.name,
                description: item.description,
                location: item.path,
                relative_location: None,
                is_custom: true,
                source: SkillSource::Custom,
            });
        }
    }

    custom_skills.sort_by(|a, b| {
        skill_modified_time(&b.location)
            .cmp(&skill_modified_time(&a.location))
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut builtin_items: Vec<SkillListItem> = builtin_skills.into_values().collect();
    builtin_items.sort_by(|a, b| a.name.cmp(&b.name));

    let mut result = custom_skills;
    result.extend(builtin_items);
    Ok(result)
}

/// Emit a [`SkillListItem`] for every built-in skill (both auto-inject
/// and opt-in). All paths resolve directly against
/// `paths.builtin_skills_dir`.
async fn list_builtin_skills(paths: &SkillPaths) -> Vec<SkillListItem> {
    list_builtin_skills_from_disk(&paths.builtin_skills_dir).await
}

async fn list_builtin_skills_from_disk(dir: &Path) -> Vec<SkillListItem> {
    let mut items = Vec::new();

    // Top-level opt-in skills (siblings of auto-inject/).
    if let Ok(top) = scan_skill_dirs(dir).await {
        for s in top {
            if s.name == BUILTIN_AUTO_SKILLS_SUBDIR {
                continue;
            }
            // Use the on-disk directory name (basename of scanned path)
            // rather than the frontmatter name, so the path we emit
            // matches the real filesystem layout when the two disagree.
            let dir_name = Path::new(&s.path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&s.name)
                .to_string();
            let rel = format!("{dir_name}/{SKILL_MANIFEST_FILE}");
            let location = dir
                .join(&dir_name)
                .join(SKILL_MANIFEST_FILE)
                .to_string_lossy()
                .into_owned();
            items.push(SkillListItem {
                name: s.name,
                description: s.description,
                location,
                relative_location: Some(rel),
                is_custom: false,
                source: SkillSource::Builtin,
            });
        }
    }

    // auto-inject children.
    let auto_dir = dir.join(BUILTIN_AUTO_SKILLS_SUBDIR);
    if let Ok(auto) = scan_skill_dirs(&auto_dir).await {
        for s in auto {
            let dir_name = Path::new(&s.path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&s.name)
                .to_string();
            let rel = format!("{BUILTIN_AUTO_SKILLS_SUBDIR}/{dir_name}/{SKILL_MANIFEST_FILE}");
            let location = auto_dir
                .join(&dir_name)
                .join(SKILL_MANIFEST_FILE)
                .to_string_lossy()
                .into_owned();
            items.push(SkillListItem {
                name: s.name,
                description: s.description,
                location,
                relative_location: Some(rel),
                is_custom: false,
                source: SkillSource::Builtin,
            });
        }
    }

    items
}

/// A skill discovered during directory scanning.
#[derive(Debug, Clone, PartialEq)]
pub struct ScannedSkill {
    pub name: String,
    pub description: String,
    pub path: String,
}

/// An auto-injected built-in skill.
///
/// Returned by `GET /api/skills/builtin-auto`. `location` is the
/// relative path the frontend passes back into
/// `POST /api/skills/builtin-skill`, e.g. `"auto-inject/cron/SKILL.md"`.
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltinAutoSkillItem {
    pub name: String,
    pub description: String,
    pub location: String,
}

/// List built-in skills that are auto-injected into every assistant.
///
/// Reads from `{paths.builtin_skills_dir}/auto-inject/`. A missing
/// `auto-inject/` directory yields an empty list, matching the
/// graceful-degradation semantics used elsewhere in this module.
pub async fn list_builtin_auto_skills(
    paths: &SkillPaths,
) -> Result<Vec<BuiltinAutoSkillItem>, ExtensionError> {
    let auto_dir = paths.builtin_skills_dir.join(BUILTIN_AUTO_SKILLS_SUBDIR);
    let mut items = list_auto_skills_from_disk(&auto_dir).await;
    items.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(items)
}

/// Built-in skill → (audience_tags, scenario_tags) seed map, loaded from the
/// embedded `skill-tags.json`. Graceful: missing/malformed → empty map.
pub fn load_builtin_skill_tags() -> std::collections::HashMap<String, (Vec<String>, Vec<String>)> {
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
        #[serde(default)]
        audience_tags: Vec<String>,
        #[serde(default)]
        scenario_tags: Vec<String>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Manifest {
        #[serde(default)]
        skills: Vec<Entry>,
    }
    let bytes = match BUILTIN_SKILLS.get_file("skill-tags.json") {
        Some(f) => f.contents(),
        None => return std::collections::HashMap::new(),
    };
    let manifest: Manifest = serde_json::from_slice(bytes).unwrap_or_default();
    manifest
        .skills
        .into_iter()
        .map(|e| (e.name, (e.audience_tags, e.scenario_tags)))
        .collect()
}

async fn list_auto_skills_from_disk(auto_dir: &Path) -> Vec<BuiltinAutoSkillItem> {
    let entries = match scan_skill_dirs(auto_dir).await {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    entries
        .into_iter()
        .map(|s| {
            let name = s.name.clone();
            BuiltinAutoSkillItem {
                name,
                description: s.description,
                location: format!(
                    "{BUILTIN_AUTO_SKILLS_SUBDIR}/{}/{SKILL_MANIFEST_FILE}",
                    s.name
                ),
            }
        })
        .collect()
}

/// Read skill info from a SKILL.md file without importing.
///
/// Returns `(name, description)` extracted from frontmatter.
pub async fn read_skill_info(skill_path: &Path) -> Result<(String, String), ExtensionError> {
    let skill_file = if skill_path.is_dir() {
        skill_path.join(SKILL_MANIFEST_FILE)
    } else {
        skill_path.to_path_buf()
    };

    let content = tokio::fs::read_to_string(&skill_file)
        .await
        .map_err(|_| ExtensionError::SkillNotFound(skill_path.display().to_string()))?;

    let (name, description) = parse_frontmatter_fields(&content).ok_or_else(|| {
        ExtensionError::InvalidSkillPath(format!(
            "No valid frontmatter in {}",
            skill_file.display()
        ))
    })?;

    // Fallback: use directory name if name is empty
    let final_name = if name.is_empty() {
        skill_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        name
    };

    Ok((final_name, description))
}

// ---------------------------------------------------------------------------
// D. Skill import / export / delete
// ---------------------------------------------------------------------------

/// Import a skill by copying its directory to the user skills directory.
///
/// Returns the skill name.
pub async fn import_skill(paths: &SkillPaths, skill_path: &Path) -> Result<String, ExtensionError> {
    let (name, _) = read_skill_info(skill_path).await?;
    validate_filename(&name)?;

    let target_dir = paths.user_skills_dir.join(&name);
    tokio::fs::create_dir_all(&paths.user_skills_dir).await?;

    copy_dir_recursive(skill_path, &target_dir).await?;

    debug!(skill = %name, target = %target_dir.display(), "skill imported (copy)");
    Ok(name)
}

/// Import a skill by creating a symlink in the user skills directory.
///
/// Returns the skill name.
pub async fn import_skill_with_symlink(
    paths: &SkillPaths,
    skill_path: &Path,
) -> Result<String, ExtensionError> {
    let (name, _) = read_skill_info(skill_path).await?;
    validate_filename(&name)?;

    let target_link = paths.user_skills_dir.join(&name);
    tokio::fs::create_dir_all(&paths.user_skills_dir).await?;

    // Remove existing link/dir if present
    if target_link.exists() {
        if target_link.is_symlink() || target_link.is_file() {
            tokio::fs::remove_file(&target_link).await?;
        } else {
            tokio::fs::remove_dir_all(&target_link).await?;
        }
    }

    // Materialize the link, degrading to a recursive copy when the platform
    // symlink/junction primitive fails (non-NTFS removable media, UNC/network
    // source, path-too-long, locked target, or Windows symlink privilege).
    // Mirrors the resilience the per-agent path already has via
    // `link_workspace_skills`; without it these failures surfaced as an opaque
    // 500 "导入技能出错".
    link_skill_or_fallback_copy(skill_path, &target_link).await?;

    debug!(skill = %name, link = %target_link.display(), "skill imported (symlink)");
    Ok(name)
}

/// Maximum directory depth descended when scanning a user-selected folder (or
/// extracted zip) for skills. Bounds the walk so picking a huge tree (e.g. a
/// drive root) cannot trigger a full-disk scan. Descent stops early at any
/// directory containing `SKILL.md`, so this only caps `SKILL.md`-less nesting.
const MAX_IMPORT_SCAN_DEPTH: usize = 6;

/// Import one skill, a parent directory containing skills, or a zip archive.
///
/// Directory inputs preserve the existing symlink behavior. Zip inputs are
/// extracted into an internal temporary directory, then copied into the user
/// skills directory so imported skills do not point at disposable files.
///
/// A directory that is not itself a skill is scanned **recursively** (bounded
/// by [`MAX_IMPORT_SCAN_DEPTH`]) so the user can pick a parent/grandparent or a
/// nested skill bundle — not just a folder whose immediate children are skills.
/// Multi-skill imports are **best-effort**: one malformed skill is skipped
/// (with a warning) rather than aborting the whole import and leaving the
/// already-linked skills orphaned. An error is only returned when *no* skill
/// could be imported, preserving the precise reason for the single-skill case.
pub async fn import_skills_with_symlink(
    paths: &SkillPaths,
    source_path: &Path,
) -> Result<Vec<String>, ExtensionError> {
    if is_zip_path(source_path) {
        return import_skills_from_zip(paths, source_path).await;
    }

    let source_path = normalize_import_source_path(source_path)?;

    if source_path.is_dir() {
        if source_path.join(SKILL_MANIFEST_FILE).exists() {
            return Ok(vec![import_skill_with_symlink(paths, &source_path).await?]);
        }

        let mut skill_dirs = Vec::new();
        collect_skill_dirs_recursive(&source_path, &mut skill_dirs, MAX_IMPORT_SCAN_DEPTH).await?;
        if skill_dirs.is_empty() {
            return Err(ExtensionError::InvalidSkillPath(format!(
                "No skill directories found in {}",
                source_path.display()
            )));
        }

        let mut imported = Vec::new();
        let mut last_err: Option<ExtensionError> = None;
        for skill_dir in &skill_dirs {
            match import_skill_with_symlink(paths, skill_dir).await {
                Ok(name) => imported.push(name),
                Err(e) => {
                    warn!(skill_dir = %skill_dir.display(), error = %e, "skipping skill that failed to import");
                    last_err = Some(e);
                }
            }
        }
        if imported.is_empty() {
            return Err(last_err.unwrap_or_else(|| {
                ExtensionError::InvalidSkillPath(format!(
                    "No importable skills found in {}",
                    source_path.display()
                ))
            }));
        }
        imported.sort();
        imported.dedup();
        return Ok(imported);
    }

    Err(ExtensionError::InvalidSkillPath(format!(
        "Expected a skill directory, parent directory, SKILL.md, or zip archive: {}",
        source_path.display()
    )))
}

async fn import_skills_from_zip(
    paths: &SkillPaths,
    archive_path: &Path,
) -> Result<Vec<String>, ExtensionError> {
    let temp_root = paths.user_skills_dir.join(".import-tmp");
    tokio::fs::create_dir_all(&temp_root).await?;

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let extract_dir = temp_root.join(format!("skills-{}-{nonce}", std::process::id()));
    tokio::fs::create_dir_all(&extract_dir).await?;

    let archive = archive_path.to_path_buf();
    let destination = extract_dir.clone();
    let extraction =
        tokio::task::spawn_blocking(move || extract_zip_archive(&archive, &destination))
            .await
            .map_err(|e| {
                ExtensionError::InvalidSkillPath(format!("Zip extraction task failed: {e}"))
            })?;

    if let Err(err) = extraction {
        let _ = tokio::fs::remove_dir_all(&extract_dir).await;
        let _ = tokio::fs::remove_dir(&temp_root).await;
        return Err(err);
    }

    let result = async {
        let mut skill_dirs = Vec::new();
        collect_skill_dirs_recursive(&extract_dir, &mut skill_dirs, MAX_IMPORT_SCAN_DEPTH).await?;
        if skill_dirs.is_empty() {
            return Err(ExtensionError::InvalidSkillPath(format!(
                "No skill directories found in {}",
                archive_path.display()
            )));
        }

        let mut imported = Vec::new();
        for skill_dir in skill_dirs {
            imported.push(import_skill(paths, &skill_dir).await?);
        }
        imported.sort();
        imported.dedup();
        Ok(imported)
    }
    .await;

    let _ = tokio::fs::remove_dir_all(&extract_dir).await;
    let _ = tokio::fs::remove_dir(&temp_root).await;
    result
}

fn is_zip_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

fn skill_modified_time(path: &str) -> SystemTime {
    std::fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(UNIX_EPOCH)
}

fn normalize_import_source_path(source_path: &Path) -> Result<PathBuf, ExtensionError> {
    if source_path.is_file() {
        let file_name = source_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if file_name == SKILL_MANIFEST_FILE {
            return source_path.parent().map(Path::to_path_buf).ok_or_else(|| {
                ExtensionError::InvalidSkillPath(source_path.display().to_string())
            });
        }
    }
    Ok(source_path.to_path_buf())
}

/// Export a skill by creating a symlink in the target directory.
pub async fn export_skill_with_symlink(
    skill_path: &Path,
    target_dir: &Path,
) -> Result<(), ExtensionError> {
    let skill_name = skill_path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .ok_or_else(|| ExtensionError::InvalidSkillPath(skill_path.display().to_string()))?;

    let target_link = target_dir.join(&skill_name);
    tokio::fs::create_dir_all(target_dir).await?;

    // Remove existing link if present
    if target_link.exists() {
        if target_link.is_symlink() || target_link.is_file() {
            tokio::fs::remove_file(&target_link).await?;
        } else {
            tokio::fs::remove_dir_all(&target_link).await?;
        }
    }

    create_symlink(skill_path, &target_link).await?;

    debug!(
        skill = %skill_name,
        link = %target_link.display(),
        "skill exported (symlink)"
    );
    Ok(())
}

/// Delete a user-custom skill by name.
///
/// Returns an error if the skill is built-in or does not exist.
pub async fn delete_skill(paths: &SkillPaths, skill_name: &str) -> Result<(), ExtensionError> {
    // Safety: reject path traversal
    if skill_name.contains('/') || skill_name.contains('\\') || skill_name.contains("..") {
        return Err(ExtensionError::PathTraversal(skill_name.to_string()));
    }

    let user_path = paths.user_skills_dir.join(skill_name);

    if !user_path.exists() {
        // Check if it exists as a built-in (disk override → filesystem,
        // otherwise embedded corpus).
        if builtin_skill_exists(paths, skill_name) {
            return Err(ExtensionError::BuiltinSkillDeletion(skill_name.to_string()));
        }
        return Err(ExtensionError::SkillNotFound(skill_name.to_string()));
    }

    if user_path.is_symlink() || user_path.is_file() {
        tokio::fs::remove_file(&user_path).await?;
    } else {
        tokio::fs::remove_dir_all(&user_path).await?;
    }

    debug!(skill = %skill_name, "skill deleted");
    Ok(())
}

/// Check whether a skill name exists in the built-in corpus — either as
/// a top-level opt-in skill or under `auto-inject/`. Consults the
/// on-disk tree at `paths.builtin_skills_dir`.
fn builtin_skill_exists(paths: &SkillPaths, skill_name: &str) -> bool {
    paths.builtin_skills_dir.join(skill_name).is_dir()
        || paths
            .builtin_skills_dir
            .join(BUILTIN_AUTO_SKILLS_SUBDIR)
            .join(skill_name)
            .is_dir()
}

// ---------------------------------------------------------------------------
// D2. Per-agent skill resolution
// ---------------------------------------------------------------------------

/// A resolved skill reference returned by [`materialize_skills_for_agent`].
///
/// `name` is the skill's requested name; `source_path` is the absolute
/// on-disk directory containing its `SKILL.md`. The caller is expected
/// to symlink that directory into the agent CLI's native skills dir
/// rather than copy it — backend no longer owns per-conversation files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgentSkill {
    pub name: String,
    pub source_path: PathBuf,
}

/// Resolve each requested skill name to its on-disk source directory.
///
/// Search order per name (first match wins):
/// 1. `{builtin_skills_dir}/{name}/` — top-level opt-in builtin.
/// 2. `{builtin_skills_dir}/auto-inject/{name}/` — auto-inject builtin.
/// 3. `{user_skills_dir}/{name}/` — user-created custom skill.
/// 4. `{cron_skills_dir}/{name}/` — per-job cron skill.
///
/// No files are copied and no per-conversation directory is created —
/// the backend just hands the absolute source paths back to the caller,
/// which is responsible for symlinking them where the CLI expects. This
/// replaces the older "copy into `{data_dir}/agent-skills/{conv_id}/`"
/// behavior once the frontend moved to a symlink-only contract.
///
/// Unknown names are silently skipped (a warning is emitted). Names
/// containing path separators or `..` are rejected with a warn and
/// skipped, matching the legacy behavior. Empty names are ignored.
///
/// The returned list is sorted by `name` for determinism. The
/// `conversation_id` is still validated (rejects path-traversal values)
/// so downstream callers can safely use it in log lines or paths even
/// though this function no longer touches disk per-conversation.
pub async fn materialize_skills_for_agent(
    paths: &SkillPaths,
    conversation_id: &str,
    skills: &[String],
) -> Result<Vec<ResolvedAgentSkill>, ExtensionError> {
    validate_filename(conversation_id)?;

    let mut resolved = Vec::with_capacity(skills.len());
    for name in skills {
        if name.is_empty() {
            continue;
        }
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            warn!(skill = %name, "skipping skill with invalid name");
            continue;
        }
        match resolve_skill_source_path(paths, name) {
            Some(source_path) => resolved.push(ResolvedAgentSkill {
                name: name.clone(),
                source_path,
            }),
            None => warn!(skill = %name, "skill not found in any source"),
        }
    }

    resolved.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(resolved)
}

/// Create symlinks from a set of resolved skills into the agent CLI's
/// native skills directories inside `workspace`.
///
/// For each relative `skills_rel_dir` (e.g. `.claude/skills`):
/// 1. Ensure `{workspace}/{skills_rel_dir}/` exists.
/// 2. For each `{ name, source_path }` in `skills`, create a symlink
///    `{workspace}/{skills_rel_dir}/{name} -> {source_path}`.
///
/// Existing symlinks/files at the target name are left untouched
/// (first-write-wins, matches the frontend's lstat-then-skip behavior
/// before symlink creation). Individual symlink failures are logged and
/// skipped — skill discovery degrades gracefully, it is not fatal.
///
/// Returns the number of symlinks successfully created across all
/// target dirs.
pub async fn link_workspace_skills(
    workspace: &Path,
    skills_rel_dirs: &[&str],
    skills: &[ResolvedAgentSkill],
) -> Result<usize, ExtensionError> {
    let mut created = 0usize;
    for rel in skills_rel_dirs {
        let target_skills_dir = workspace.join(rel);
        tokio::fs::create_dir_all(&target_skills_dir).await?;

        for skill in skills {
            let target = target_skills_dir.join(&skill.name);
            match tokio::fs::symlink_metadata(&target).await {
                // Target already exists — leave it alone.
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(
                        target = %target.display(),
                        error = %e,
                        "skipping skill link: failed to stat target"
                    );
                    continue;
                }
            }
            match link_skill_or_fallback_copy(&skill.source_path, &target).await {
                Ok(()) => {
                    debug!(
                        skill = %skill.name,
                        target = %target.display(),
                        "linked workspace skill"
                    );
                    created += 1;
                }
                Err(e) => {
                    warn!(
                        skill = %skill.name,
                        target = %target.display(),
                        error = %e,
                        "failed to link workspace skill"
                    );
                }
            }
        }
    }
    Ok(created)
}

/// Resolve a skill name to its on-disk source directory using the same
/// search order as [`materialize_skills_for_agent`]. Returns `None` if
/// no matching directory exists in any known source.
fn resolve_skill_source_path(paths: &SkillPaths, name: &str) -> Option<PathBuf> {
    let top = paths.builtin_skills_dir.join(name);
    if top.is_dir() {
        return Some(top);
    }
    let auto = paths
        .builtin_skills_dir
        .join(BUILTIN_AUTO_SKILLS_SUBDIR)
        .join(name);
    if auto.is_dir() {
        return Some(auto);
    }
    let user = paths.user_skills_dir.join(name);
    if user.is_dir() {
        return Some(user);
    }
    let cron = paths.cron_skills_dir.join(name);
    if cron.is_dir() {
        return Some(cron);
    }
    None
}

// ---------------------------------------------------------------------------
// E. Scanning & discovery
// ---------------------------------------------------------------------------

/// Scan a directory for subdirectories containing SKILL.md.
pub async fn scan_for_skills(folder_path: &Path) -> Result<Vec<ScannedSkill>, ExtensionError> {
    scan_skill_dirs(folder_path).await
}

/// Named filesystem path.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedPath {
    pub name: String,
    pub path: String,
}

/// Detect common skill paths relative to the user's home directory.
///
/// Returns paths that actually exist on the filesystem.
pub async fn detect_common_skill_paths() -> Vec<NamedPath> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };

    let mut result = Vec::new();
    for (name, rel_path, _slug) in COMMON_SKILL_DIRS {
        let full_path = home.join(rel_path);
        if full_path.exists() {
            result.push(NamedPath {
                name: (*name).to_string(),
                path: full_path.to_string_lossy().into_owned(),
            });
        }
    }

    result
}

/// An external skill source with discovered skills.
///
/// `source` is a stable slug identifying the origin — matches the
/// `ExternalSkillSourceResponse.source` contract consumed by the renderer.
/// Values are drawn from [`COMMON_SKILL_DIRS`] for built-in entries or
/// `format!("custom-{path}")` for user-added paths, so they stay unique
/// across the returned list.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalSkillSource {
    pub name: String,
    pub path: String,
    pub source: String,
    pub skill_count: usize,
    pub skills: Vec<ScannedSkill>,
}

/// Compute the stable `source` slug for a custom external path.
fn custom_source_slug(path: &str) -> String {
    format!("custom-{path}")
}

/// Discover external skills from common paths and custom external paths.
///
/// The returned list preserves deterministic `source` slugs — see
/// [`ExternalSkillSource::source`] for the contract.
pub async fn detect_and_count_external_skills(
    custom_paths: &[NamedPath],
) -> Vec<ExternalSkillSource> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };

    let mut sources = Vec::new();

    // 1. Common paths (iterate the constant table so we keep the per-entry slug).
    for (name, rel_path, slug) in COMMON_SKILL_DIRS {
        let full_path = home.join(rel_path);
        if !full_path.exists() {
            continue;
        }
        if let Ok(skills) = scan_skill_dirs(&full_path).await {
            sources.push(ExternalSkillSource {
                name: (*name).to_string(),
                path: full_path.to_string_lossy().into_owned(),
                source: (*slug).to_string(),
                skill_count: skills.len(),
                skills,
            });
        }
    }

    // 2. Custom external paths
    for np in custom_paths {
        let path = Path::new(&np.path);
        if let Ok(skills) = scan_skill_dirs(path).await {
            sources.push(ExternalSkillSource {
                name: np.name.clone(),
                path: np.path.clone(),
                source: custom_source_slug(&np.path),
                skill_count: skills.len(),
                skills,
            });
        }
    }

    sources
}

/// Get the user and built-in skill directory paths.
///
/// Both values are real on-disk paths. The built-in path points at the
/// tree populated at startup by
/// [`crate::startup_materialize::materialize_if_needed`], or at the
/// [`BUILTIN_SKILLS_ENV_VAR`] override when set.
pub fn get_skill_paths(paths: &SkillPaths) -> (String, String) {
    (
        paths.user_skills_dir.to_string_lossy().into_owned(),
        paths.builtin_skills_dir.to_string_lossy().into_owned(),
    )
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Read a file and return its content, or an empty string if it does not exist.
async fn read_file_or_empty(path: &Path) -> Result<String, ExtensionError> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(ExtensionError::Io(e)),
    }
}

/// Validate a filename to prevent path traversal.
fn validate_filename(name: &str) -> Result<(), ExtensionError> {
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.is_empty() {
        return Err(ExtensionError::PathTraversal(name.to_string()));
    }
    Ok(())
}

/// Validate a relative path inside the built-in skill corpus. Allows
/// forward slashes (paths like `"auto-inject/cron/SKILL.md"` are
/// normal) but forbids empty segments, backslashes, leading slash,
/// absolute paths, and any `..` component.
fn validate_builtin_skill_path(rel: &str) -> Result<(), ExtensionError> {
    if rel.is_empty() || rel.contains('\\') || rel.contains("..") || rel.starts_with('/') {
        return Err(ExtensionError::PathTraversal(rel.to_string()));
    }
    if rel.split('/').any(|seg| seg.is_empty()) {
        return Err(ExtensionError::PathTraversal(rel.to_string()));
    }
    if Path::new(rel).is_absolute() {
        return Err(ExtensionError::PathTraversal(rel.to_string()));
    }
    Ok(())
}

/// Read an assistant resource (rule or skill) with locale fallback.
async fn read_assistant_resource(
    dir: &Path,
    assistant_id: &str,
    locale: Option<&str>,
) -> Result<String, ExtensionError> {
    validate_filename(assistant_id)?;
    if let Some(loc) = locale {
        validate_filename(loc)?;
    }

    // 1. Try locale-specific file
    if let Some(loc) = locale
        && !loc.is_empty()
    {
        let locale_file = dir.join(format!("{assistant_id}.{loc}.md"));
        if let Ok(content) = tokio::fs::read_to_string(&locale_file).await {
            return Ok(content);
        }
    }

    // 2. Try default file (no locale suffix)
    let default_file = dir.join(format!("{assistant_id}.md"));
    match tokio::fs::read_to_string(&default_file).await {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(ExtensionError::Io(e)),
    }
}

/// Write an assistant resource file.
async fn write_assistant_resource(
    dir: &Path,
    assistant_id: &str,
    content: &str,
    locale: Option<&str>,
) -> Result<bool, ExtensionError> {
    validate_filename(assistant_id)?;
    if let Some(loc) = locale {
        validate_filename(loc)?;
    }

    tokio::fs::create_dir_all(dir).await?;

    let filename = match locale {
        Some(loc) if !loc.is_empty() => format!("{assistant_id}.{loc}.md"),
        _ => format!("{assistant_id}.md"),
    };

    let file_path = dir.join(filename);
    tokio::fs::write(&file_path, content).await?;
    debug!(path = %file_path.display(), "assistant resource written");
    Ok(true)
}

/// Delete all files matching `{assistant_id}*.md` in a directory.
async fn delete_assistant_resource(dir: &Path, assistant_id: &str) -> Result<bool, ExtensionError> {
    validate_filename(assistant_id)?;

    let mut deleted_any = false;

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(ExtensionError::Io(e)),
    };

    let prefix = format!("{assistant_id}.");
    let exact = format!("{assistant_id}.md");

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == exact || (name.starts_with(&prefix) && name.ends_with(".md")) {
            tokio::fs::remove_file(entry.path()).await?;
            deleted_any = true;
            debug!(file = %name, "deleted assistant resource");
        }
    }

    Ok(deleted_any)
}

/// Scan a directory for subdirectories containing a SKILL.md file.
async fn scan_skill_dirs(dir: &Path) -> Result<Vec<ScannedSkill>, ExtensionError> {
    let mut result = Vec::new();

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Err(e) => return Err(ExtensionError::Io(e)),
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let entry_path = entry.path();
        if !entry_path.is_dir() {
            continue;
        }

        let skill_file = entry_path.join(SKILL_MANIFEST_FILE);
        if !skill_file.exists() {
            continue;
        }

        match tokio::fs::read_to_string(&skill_file).await {
            Ok(content) => {
                if let Some((name, description)) = parse_frontmatter_fields(&content) {
                    let final_name = if name.is_empty() {
                        entry_path
                            .file_name()
                            .map(|f| f.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    } else {
                        name
                    };
                    result.push(ScannedSkill {
                        name: final_name,
                        description,
                        path: entry_path.to_string_lossy().into_owned(),
                    });
                } else {
                    warn!(
                        path = %skill_file.display(),
                        "skipping skill: SKILL.md has no valid frontmatter (missing/empty description?)"
                    );
                }
            }
            Err(e) => {
                warn!(
                    path = %skill_file.display(),
                    error = %e,
                    "failed to read SKILL.md"
                );
            }
        }
    }

    result.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}

async fn collect_skill_dirs_recursive(
    dir: &Path,
    result: &mut Vec<PathBuf>,
    max_depth: usize,
) -> Result<(), ExtensionError> {
    if dir.join(SKILL_MANIFEST_FILE).exists() {
        result.push(dir.to_path_buf());
        return Ok(());
    }

    if max_depth == 0 {
        return Ok(());
    }

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(ExtensionError::Io(e)),
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let entry_path = entry.path();
        if entry_path.is_dir() {
            Box::pin(collect_skill_dirs_recursive(
                &entry_path,
                result,
                max_depth - 1,
            ))
            .await?;
        }
    }

    result.sort();
    Ok(())
}

fn extract_zip_archive(archive_path: &Path, destination: &Path) -> Result<(), ExtensionError> {
    let file = std::fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(zip_error)?;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(zip_error)?;
        let entry_name = entry.name().to_string();
        reject_zip_symlink(&entry)?;
        let relative_path = safe_zip_entry_path(&entry_name)?;
        let output_path = destination.join(relative_path);

        if entry.is_dir() {
            std::fs::create_dir_all(&output_path)?;
            continue;
        }

        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output = std::fs::File::create(&output_path)?;
        io::copy(&mut entry, &mut output)?;
    }

    Ok(())
}

fn safe_zip_entry_path(name: &str) -> Result<PathBuf, ExtensionError> {
    if name.is_empty() || name.contains('\\') {
        return Err(ExtensionError::PathTraversal(name.to_string()));
    }

    let path = Path::new(name);
    if path.is_absolute() {
        return Err(ExtensionError::PathTraversal(name.to_string()));
    }

    let mut safe_path = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => safe_path.push(part),
            Component::CurDir => {}
            _ => return Err(ExtensionError::PathTraversal(name.to_string())),
        }
    }

    if safe_path.as_os_str().is_empty() {
        return Err(ExtensionError::PathTraversal(name.to_string()));
    }

    Ok(safe_path)
}

fn reject_zip_symlink(entry: &zip::read::ZipFile<'_>) -> Result<(), ExtensionError> {
    if let Some(mode) = entry.unix_mode()
        && mode & 0o170000 == 0o120000
    {
        return Err(ExtensionError::PathTraversal(entry.name().to_string()));
    }
    Ok(())
}

fn zip_error(err: zip::result::ZipError) -> ExtensionError {
    ExtensionError::InvalidSkillPath(format!("Invalid zip archive: {err}"))
}

/// Parse SKILL.md frontmatter to extract name and description.
///
/// Expected format:
/// ```text
/// ---
/// name: skill-name
/// description: One line description
/// ---
/// Body content here...
/// ```
///
/// `description` may also be a YAML block scalar (`|` / `>`, with optional
/// chomping indicators) or have its value on the following indented line(s);
/// such continuations are gathered into the value rather than mis-read as the
/// literal indicator `"|"`/`">"` or dropped entirely. Surrounding quotes on a
/// single-line value are stripped. A `description` that resolves to empty (or
/// is absent) yields `None` — the description is the agent's trigger text and
/// is treated as required, consistent with the Skills spec.
fn parse_frontmatter_fields(content: &str) -> Option<(String, String)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }

    // Closing fence must be on its own line so a `---` inside a value doesn't
    // truncate the block.
    let after_open = &trimmed[3..];
    let close_idx = after_open.find("\n---")?;
    let frontmatter = &after_open[..close_idx];

    let mut name = String::new();
    let mut description = String::new();

    let lines: Vec<&str> = frontmatter.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        i += 1;

        if line.is_empty() {
            continue;
        }

        if let Some(val) = strip_yaml_key(line, "name") {
            name = unquote(val.trim()).to_string();
        } else if let Some(val) = strip_yaml_key(line, "description") {
            let val = val.trim();
            let is_block = val.starts_with('|') || val.starts_with('>');
            if is_block || val.is_empty() {
                // Folded (`>` or plain next-line) joins with spaces; literal
                // (`|`) preserves line breaks.
                let folded = !val.starts_with('|');
                let mut collected: Vec<String> = Vec::new();
                while i < lines.len() {
                    let cont = lines[i];
                    if cont.trim().is_empty() {
                        collected.push(String::new());
                        i += 1;
                        continue;
                    }
                    // Continuation lines are indented; a column-0 line starts a
                    // new key and ends the block.
                    if cont.len() == cont.trim_start().len() {
                        break;
                    }
                    collected.push(cont.trim().to_string());
                    i += 1;
                }
                while collected.last().is_some_and(|s| s.is_empty()) {
                    collected.pop();
                }
                description = if folded {
                    collected
                        .join(" ")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    collected.join("\n")
                };
            } else {
                description = unquote(val).to_string();
            }
        }
    }

    if description.trim().is_empty() {
        return None;
    }

    Some((name, description))
}

/// Strip a `key:` prefix from an already-trimmed line, returning the raw value
/// (which may be empty). Rejects look-alike keys such as `namespace:` for
/// `name`.
fn strip_yaml_key<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.strip_prefix(key)?.strip_prefix(':')
}

/// Remove a single pair of matching surrounding quotes, if present.
fn unquote(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Recursively copy a directory.
async fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), ExtensionError> {
    tokio::fs::create_dir_all(dst).await?;

    let mut entries = tokio::fs::read_dir(src).await?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let entry_path = entry.path();
        let dest_path = dst.join(entry.file_name());

        if entry_path.is_dir() {
            Box::pin(copy_dir_recursive(&entry_path, &dest_path)).await?;
        } else {
            tokio::fs::copy(&entry_path, &dest_path).await?;
        }
    }

    Ok(())
}

/// Try to symlink `src` into `dst`; on failure, fall back to a recursive
/// copy of the source directory.
///
/// Motivation: on Windows machines without "Developer Mode" or admin
/// privileges, `CreateSymbolicLinkW` fails with `os error 1314`
/// (`ERROR_PRIVILEGE_NOT_HELD`). Auto-injected builtin skills under each
/// backend's `.<backend>/skills/` directory then become invisible to the
/// CLI agent — silently degrading the product. Falling back to a copy
/// keeps the skills discoverable; the trade-off is that copies do not
/// track upstream changes until the next link pass clears them. The
/// fallback applies on every platform (Linux/macOS shouldn't normally
/// hit this, but we keep behavior uniform so a future EPERM/EROFS sandbox
/// also stays healthy).
///
/// Logs a `warn!` with the OS error kind and `raw_os_error` so we can
/// keep tracking 1314 vs other failure modes in telemetry. No
/// user-identifying data is logged — only the source/target paths
/// (already considered safe to log elsewhere in this module) and the
/// error code.
async fn link_skill_or_fallback_copy(src: &Path, dst: &Path) -> Result<(), ExtensionError> {
    match create_symlink_for_link(src, dst).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // Surface the raw OS error so dashboards can keep counting 1314
            // (ERROR_PRIVILEGE_NOT_HELD) separately from other failure modes.
            let raw_os_error = match &e {
                ExtensionError::Io(io_err) => io_err.raw_os_error(),
                _ => None,
            };
            warn!(
                src = %src.display(),
                dst = %dst.display(),
                error = %e,
                raw_os_error = ?raw_os_error,
                "create_symlink failed; falling back to copy_dir_recursive"
            );
            copy_dir_recursive(src, dst).await
        }
    }
}

/// Wrapper around [`create_symlink`] that allows tests to inject a
/// synthetic failure. In non-test builds this is a thin call-through to
/// the platform-specific [`create_symlink`] below.
async fn create_symlink_for_link(src: &Path, dst: &Path) -> Result<(), ExtensionError> {
    #[cfg(test)]
    {
        if test_overrides::should_force_symlink_failure() {
            // Use PermissionDenied to mimic the shape Windows returns
            // for ERROR_PRIVILEGE_NOT_HELD. The exact raw_os_error is
            // platform-specific so we only assert on kind in tests.
            return Err(ExtensionError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "forced symlink failure (test)",
            )));
        }
    }
    create_symlink(src, dst).await
}

/// Test-only knob to force the symlink primitive to fail, exercising
/// the [`copy_dir_recursive`] fallback branch on platforms where
/// symlinking would otherwise succeed (Linux/macOS CI).
#[cfg(test)]
mod test_overrides {
    use std::sync::atomic::{AtomicBool, Ordering};

    static FORCE_SYMLINK_FAILURE: AtomicBool = AtomicBool::new(false);

    pub fn should_force_symlink_failure() -> bool {
        FORCE_SYMLINK_FAILURE.load(Ordering::SeqCst)
    }

    /// RAII guard that flips `FORCE_SYMLINK_FAILURE` on creation and
    /// resets it on drop. Tests using this guard must be marked
    /// `#[serial_test::serial]` if any other test in the binary also
    /// flips the flag — at present only one test uses it, so a guard
    /// is enough.
    pub struct ForceFailureGuard;

    impl ForceFailureGuard {
        pub fn new() -> Self {
            FORCE_SYMLINK_FAILURE.store(true, Ordering::SeqCst);
            Self
        }
    }

    impl Drop for ForceFailureGuard {
        fn drop(&mut self) {
            FORCE_SYMLINK_FAILURE.store(false, Ordering::SeqCst);
        }
    }
}

/// Create a symlink (platform-aware).
#[cfg(unix)]
async fn create_symlink(src: &Path, dst: &Path) -> Result<(), ExtensionError> {
    tokio::fs::symlink(src, dst)
        .await
        .map_err(ExtensionError::Io)
}

#[cfg(windows)]
async fn create_symlink(src: &Path, dst: &Path) -> Result<(), ExtensionError> {
    // On Windows, directory symlinks require `SeCreateSymbolicLink`
    // (Developer Mode or Admin), which most users don't have — this is
    // the source of the Sentry I1 family of `os error 1314` failures.
    //
    // NTFS junctions are an unprivileged alternative for *directory*
    // targets: the kernel exposes them via `FSCTL_SET_REPARSE_POINT`
    // which does not require the symlink privilege. Use them whenever
    // possible. File targets cannot be junctioned, so they fall back to
    // `tokio::fs::symlink_file`; in the rare cases that fails the
    // outer `link_skill_or_fallback_copy` wrapper still rescues us via
    // `copy_dir_recursive`.
    if src.is_dir() {
        let src = src.to_path_buf();
        let dst = dst.to_path_buf();
        tokio::task::spawn_blocking(move || junction::create(&src, &dst))
            .await
            .map_err(|e| {
                ExtensionError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("junction::create join error: {e}"),
                ))
            })?
            .map_err(ExtensionError::Io)
    } else {
        tokio::fs::symlink_file(src, dst)
            .await
            .map_err(ExtensionError::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn builtin_skills_materialize_version_includes_corpus_fingerprint() {
        let fingerprint = builtin_skills_corpus_fingerprint();
        assert_eq!(fingerprint.len(), 64);
        assert!(fingerprint.chars().all(|ch| ch.is_ascii_hexdigit()));

        let version = builtin_skills_materialize_version("1.2.3");
        assert_eq!(version, format!("1.2.3+skills.{}", &fingerprint[..12]));
    }

    /// Build a `SkillPaths` rooted at a temp dir for self-evolution path/write tests.
    fn test_paths(tmp: &TempDir) -> SkillPaths {
        SkillPaths {
            data_dir: tmp.path().to_path_buf(),
            user_skills_dir: tmp.path().join(SKILLS_DIR_NAME),
            cron_skills_dir: tmp.path().join(CRON_SKILLS_DIR_NAME),
            builtin_skills_dir: tmp.path().join("builtin-skills"),
            builtin_rules_dir: tmp.path().join("rules"),
            assistant_rules_dir: tmp.path().join("assistant-rules"),
            assistant_skills_dir: tmp.path().join("assistant-skills"),
        }
    }

    #[test]
    fn skill_dir_for_scopes_and_rejects_traversal() {
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);
        let active =
            skill_dir_for(&paths, &SkillScope::Companion("c1".into()), "weekly", false).unwrap();
        assert!(
            active.ends_with("skills/companion/c1/weekly"),
            "{}",
            active.display()
        );
        let draft =
            skill_dir_for(&paths, &SkillScope::Companion("c1".into()), "weekly", true).unwrap();
        assert!(
            draft.ends_with("skills/_drafts/c1/weekly"),
            "{}",
            draft.display()
        );
        let shared = skill_dir_for(&paths, &SkillScope::Shared, "fmt", false).unwrap();
        assert!(
            shared.ends_with("skills/shared/fmt"),
            "{}",
            shared.display()
        );
        assert!(
            skill_dir_for(
                &paths,
                &SkillScope::Companion("../x".into()),
                "weekly",
                false
            )
            .is_err()
        );
        assert!(
            skill_dir_for(
                &paths,
                &SkillScope::Companion("c1".into()),
                "../escape",
                false
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn create_skill_writes_valid_manifest_and_rejects_empty_desc() {
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);
        let input = SkillDraftInput {
            name: "weekly-report".into(),
            description: "把本周工作汇总成周报".into(),
            when_to_use: Some("当用户说‘出周报’或周五收尾时".into()),
            allowed_tools: None,
            paths: None,
            body: "## 步骤\n1. 收集本周已完成任务\n2. 按项目归类\n3. 生成 markdown 周报".into(),
        };
        let dir = create_skill(&paths, &SkillScope::Companion("c1".into()), true, &input)
            .await
            .unwrap();
        let manifest = dir.join(SKILL_MANIFEST_FILE);
        assert!(manifest.exists());
        // 能被既有 read_skill_info 正确回读（frontmatter 合法）
        let (name, desc) = read_skill_info(&dir).await.unwrap();
        assert_eq!(name, "weekly-report");
        assert_eq!(desc, "把本周工作汇总成周报");
        // 空 description 必须拒
        let bad = SkillDraftInput {
            description: "".into(),
            ..input.clone()
        };
        assert!(
            create_skill(&paths, &SkillScope::Companion("c1".into()), true, &bad)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn write_skill_overwrites_and_validates() {
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);
        let md = "---\nname: fmt\ndescription: 统一代码风格\n---\n\n步骤略\n";
        write_skill(&paths, &SkillScope::Shared, false, "fmt", md)
            .await
            .unwrap();
        let dir = skill_dir_for(&paths, &SkillScope::Shared, "fmt", false).unwrap();
        let (name, desc) = read_skill_info(&dir).await.unwrap();
        assert_eq!(name, "fmt");
        assert_eq!(desc, "统一代码风格");
        // 缺 frontmatter / 空 description → 拒
        assert!(
            write_skill(
                &paths,
                &SkillScope::Shared,
                false,
                "fmt",
                "no frontmatter here"
            )
            .await
            .is_err()
        );
        assert!(
            write_skill(
                &paths,
                &SkillScope::Shared,
                false,
                "fmt",
                "---\nname: fmt\ndescription:\n---\nx"
            )
            .await
            .is_err()
        );
    }

    // -----------------------------------------------------------------------
    // Built-in skill tag seed
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn load_builtin_skill_tags_has_known_entries() {
        let m = super::load_builtin_skill_tags();
        assert!(!m.is_empty());
        let mermaid = m.get("mermaid").expect("mermaid seeded");
        assert!(mermaid.1.iter().any(|s| s == "dataviz"));
    }

    // -----------------------------------------------------------------------
    // Frontmatter parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_frontmatter_valid() {
        let content = "---\nname: my-skill\ndescription: A useful skill\n---\nBody content here.";
        let (name, desc) = parse_frontmatter_fields(content).unwrap();
        assert_eq!(name, "my-skill");
        assert_eq!(desc, "A useful skill");
    }

    #[test]
    fn parse_frontmatter_empty_name() {
        let content = "---\nname: \ndescription: Has description\n---\nBody";
        let (name, desc) = parse_frontmatter_fields(content).unwrap();
        assert!(name.is_empty());
        assert_eq!(desc, "Has description");
    }

    #[test]
    fn parse_frontmatter_no_opening() {
        let content = "name: test\ndescription: desc\n---\nbody";
        assert!(parse_frontmatter_fields(content).is_none());
    }

    #[test]
    fn parse_frontmatter_no_closing() {
        let content = "---\nname: test\ndescription: desc";
        assert!(parse_frontmatter_fields(content).is_none());
    }

    #[test]
    fn parse_frontmatter_missing_description() {
        let content = "---\nname: test\n---\nbody";
        assert!(parse_frontmatter_fields(content).is_none());
    }

    #[test]
    fn parse_frontmatter_block_scalar_pipe() {
        // Mirrors the shipped weixin-file-send/SKILL.md shape.
        let content = "---\nname: weixin-file-send\ndescription: |\n  Use when the user wants a local file sent back, such as \"send me the file\"\n  or \"发给我\".\n---\nBody";
        let (name, desc) = parse_frontmatter_fields(content).unwrap();
        assert_eq!(name, "weixin-file-send");
        assert!(desc.contains("Use when the user wants a local file sent back"));
        assert!(desc.contains("发给我"));
        // Literal block preserves the line break...
        assert!(desc.contains('\n'));
        // ...and the raw indicator must never leak as the value.
        assert_ne!(desc, "|");
    }

    #[test]
    fn parse_frontmatter_block_scalar_folded() {
        let content = "---\nname: folded\ndescription: >\n  first line\n  second line\n---\nBody";
        let (name, desc) = parse_frontmatter_fields(content).unwrap();
        assert_eq!(name, "folded");
        // Folded scalar joins continuation lines with a single space.
        assert_eq!(desc, "first line second line");
        assert_ne!(desc, ">");
    }

    #[test]
    fn parse_frontmatter_value_on_next_line() {
        let content = "---\nname: nextline\ndescription:\n  A description on the following indented line.\n---\nBody";
        let (name, desc) = parse_frontmatter_fields(content).unwrap();
        assert_eq!(name, "nextline");
        assert_eq!(desc, "A description on the following indented line.");
    }

    #[test]
    fn parse_frontmatter_strips_quotes() {
        let content = "---\nname: quoted\ndescription: \"A quoted description.\"\n---\nBody";
        let (_name, desc) = parse_frontmatter_fields(content).unwrap();
        assert_eq!(desc, "A quoted description.");
    }

    // -----------------------------------------------------------------------
    // Filename validation
    // -----------------------------------------------------------------------

    #[test]
    fn validate_filename_normal() {
        assert!(validate_filename("code-review.md").is_ok());
    }

    #[test]
    fn validate_filename_path_traversal() {
        assert!(validate_filename("../etc/passwd").is_err());
        assert!(validate_filename("foo/bar.md").is_err());
        assert!(validate_filename("foo\\bar.md").is_err());
    }

    #[test]
    fn validate_filename_empty() {
        assert!(validate_filename("").is_err());
    }

    // -----------------------------------------------------------------------
    // Built-in resource reading
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn read_builtin_rule_existing_file() {
        let tmp = TempDir::new().unwrap();
        let rules_dir = tmp.path().join(BUILTIN_RULES_DIR_NAME);
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("code-review.md"), "# Review rules").unwrap();

        let paths = SkillPaths {
            data_dir: tmp.path().to_path_buf(),
            user_skills_dir: tmp.path().join(SKILLS_DIR_NAME),
            cron_skills_dir: tmp.path().join(CRON_SKILLS_DIR_NAME),
            builtin_skills_dir: tmp.path().join(crate::constants::BUILTIN_SKILLS_DIR_NAME),
            builtin_rules_dir: rules_dir,
            assistant_rules_dir: tmp.path().join(ASSISTANT_RULES_DIR_NAME),
            assistant_skills_dir: tmp.path().join(ASSISTANT_SKILLS_DIR_NAME),
        };

        let content = read_builtin_rule(&paths, "code-review.md").await.unwrap();
        assert_eq!(content, "# Review rules");
    }

    #[tokio::test]
    async fn read_builtin_rule_missing_file() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let content = read_builtin_rule(&paths, "nonexistent.md").await.unwrap();
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn read_builtin_rule_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let result = read_builtin_rule(&paths, "../secret.md").await;
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Assistant CRUD
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn assistant_rule_write_and_read() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        write_assistant_rule(&paths, "abc123", "Be helpful.", None)
            .await
            .unwrap();

        let content = read_assistant_rule(&paths, "abc123", None).await.unwrap();
        assert_eq!(content, "Be helpful.");
    }

    #[tokio::test]
    async fn assistant_rule_locale_fallback() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        // Write default (no locale)
        write_assistant_rule(&paths, "abc123", "Default content", None)
            .await
            .unwrap();

        // Write zh-CN locale
        write_assistant_rule(&paths, "abc123", "中文内容", Some("zh-CN"))
            .await
            .unwrap();

        // Read with matching locale
        let content = read_assistant_rule(&paths, "abc123", Some("zh-CN"))
            .await
            .unwrap();
        assert_eq!(content, "中文内容");

        // Read with non-matching locale → falls back to default
        let content = read_assistant_rule(&paths, "abc123", Some("en-US"))
            .await
            .unwrap();
        assert_eq!(content, "Default content");

        // Read without locale → default
        let content = read_assistant_rule(&paths, "abc123", None).await.unwrap();
        assert_eq!(content, "Default content");
    }

    #[tokio::test]
    async fn assistant_rule_read_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let content = read_assistant_rule(&paths, "missing", None).await.unwrap();
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn assistant_rule_delete_all_locales() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        write_assistant_rule(&paths, "abc123", "Default", None)
            .await
            .unwrap();
        write_assistant_rule(&paths, "abc123", "Chinese", Some("zh-CN"))
            .await
            .unwrap();
        write_assistant_rule(&paths, "abc123", "English", Some("en-US"))
            .await
            .unwrap();

        let deleted = delete_assistant_rule(&paths, "abc123").await.unwrap();
        assert!(deleted);

        // Verify all files are gone
        let content = read_assistant_rule(&paths, "abc123", None).await.unwrap();
        assert!(content.is_empty());
        let content = read_assistant_rule(&paths, "abc123", Some("zh-CN"))
            .await
            .unwrap();
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn assistant_skill_write_and_read() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        write_assistant_skill(&paths, "abc123", "Skill content", None)
            .await
            .unwrap();

        let content = read_assistant_skill(&paths, "abc123", None).await.unwrap();
        assert_eq!(content, "Skill content");
    }

    // -----------------------------------------------------------------------
    // Assistant CRUD — path traversal prevention
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn read_assistant_rule_rejects_traversal_id() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let result = read_assistant_rule(&paths, "../etc/passwd", None).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn read_assistant_rule_rejects_traversal_locale() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let result = read_assistant_rule(&paths, "valid-id", Some("../evil")).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn write_assistant_rule_rejects_traversal_id() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let result = write_assistant_rule(&paths, "../../escape", "content", None).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn write_assistant_rule_rejects_traversal_locale() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let result = write_assistant_rule(&paths, "valid-id", "content", Some("../bad")).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn delete_assistant_rule_rejects_traversal_id() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let result = delete_assistant_rule(&paths, "foo/bar").await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn read_assistant_skill_rejects_traversal_id() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let result = read_assistant_skill(&paths, "..\\windows", None).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn write_assistant_skill_rejects_traversal_id() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let result = write_assistant_skill(&paths, "../escape", "content", None).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn delete_assistant_skill_rejects_traversal_id() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let result = delete_assistant_skill(&paths, "a/b").await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    // -----------------------------------------------------------------------
    // Skill listing
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_skills_builtin_and_custom() {
        let tmp = TempDir::new().unwrap();
        let paths = make_disk_builtin_paths(tmp.path());
        let builtin_dir = disk_builtin_dir(&paths).to_path_buf();

        // Create builtin skills
        create_skill_in_dir(&builtin_dir, "review", "Code review skill");
        create_skill_in_dir(&builtin_dir, "debug", "Debugging skill");

        // Create custom skill (overrides review)
        create_skill_in_dir(&paths.user_skills_dir, "review", "Custom review skill");
        create_skill_in_dir(&paths.user_skills_dir, "my-skill", "My custom skill");

        let skills = list_available_skills(&paths).await.unwrap();

        assert_eq!(skills.len(), 3); // debug + review (custom) + my-skill

        let review = skills.iter().find(|s| s.name == "review").unwrap();
        assert!(review.is_custom);
        assert_eq!(review.description, "Custom review skill");
        assert_eq!(review.source, SkillSource::Custom);

        let debug_skill = skills.iter().find(|s| s.name == "debug").unwrap();
        assert!(!debug_skill.is_custom);
        assert_eq!(debug_skill.source, SkillSource::Builtin);
        assert_eq!(
            debug_skill.relative_location.as_deref(),
            Some("debug/SKILL.md")
        );

        let my_skill = skills.iter().find(|s| s.name == "my-skill").unwrap();
        assert_eq!(my_skill.source, SkillSource::Custom);
        assert!(my_skill.relative_location.is_none());
    }

    #[tokio::test]
    async fn list_skills_empty_dirs() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let skills = list_available_skills(&paths).await.unwrap();
        assert!(skills.is_empty());
    }

    // -----------------------------------------------------------------------
    // Built-in auto skills
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_builtin_auto_skills_from_disk_override() {
        let tmp = TempDir::new().unwrap();
        let paths = make_disk_builtin_paths(tmp.path());
        let builtin_dir = disk_builtin_dir(&paths).to_path_buf();
        let auto_dir = builtin_dir.join(BUILTIN_AUTO_SKILLS_SUBDIR);

        create_skill_in_dir(&auto_dir, "cron", "Schedule recurring tasks");
        create_skill_in_dir(&auto_dir, "skill-creator", "Scaffold a new skill");

        // A top-level built-in skill (NOT under auto-inject/) must be excluded.
        create_skill_in_dir(&builtin_dir, "review", "Top-level builtin");

        let autos = list_builtin_auto_skills(&paths).await.unwrap();

        assert_eq!(autos.len(), 2);
        let names: std::collections::HashSet<_> = autos.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains("cron"));
        assert!(names.contains("skill-creator"));
        assert!(!names.contains("review"));

        let cron = autos.iter().find(|s| s.name == "cron").unwrap();
        assert_eq!(cron.description, "Schedule recurring tasks");
        assert_eq!(cron.location, "auto-inject/cron/SKILL.md");
    }

    #[tokio::test]
    async fn list_builtin_auto_skills_missing_dir_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let paths = make_disk_builtin_paths(tmp.path());
        // No auto-inject/ directory created under the disk override.

        let autos = list_builtin_auto_skills(&paths).await.unwrap();
        assert!(autos.is_empty());
    }

    // -----------------------------------------------------------------------
    // Skill info
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn read_skill_info_valid() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: my-skill\ndescription: A test skill\n---\nBody",
        )
        .unwrap();

        let (name, desc) = read_skill_info(&skill_dir).await.unwrap();
        assert_eq!(name, "my-skill");
        assert_eq!(desc, "A test skill");
    }

    #[tokio::test]
    async fn read_skill_info_missing() {
        let tmp = TempDir::new().unwrap();
        let result = read_skill_info(&tmp.path().join("nonexistent")).await;
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Skill import / delete
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn import_skill_copies_directory() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        // Create source skill
        let source_dir = tmp.path().join("source-skill");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: imported\ndescription: Imported skill\n---\nBody",
        )
        .unwrap();
        std::fs::write(source_dir.join("extra.txt"), "extra data").unwrap();

        let name = import_skill(&paths, &source_dir).await.unwrap();
        assert_eq!(name, "imported");

        // Verify the skill was copied
        let imported_dir = paths.user_skills_dir.join("imported");
        assert!(imported_dir.join(SKILL_MANIFEST_FILE).exists());
        assert!(imported_dir.join("extra.txt").exists());
    }

    #[tokio::test]
    #[serial]
    async fn import_skill_with_symlink_creates_link() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let source_dir = tmp.path().join("link-skill");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: linked\ndescription: Linked skill\n---\nBody",
        )
        .unwrap();

        let name = import_skill_with_symlink(&paths, &source_dir)
            .await
            .unwrap();
        assert_eq!(name, "linked");

        let link_path = paths.user_skills_dir.join("linked");
        assert!(link_path.is_symlink());
    }

    #[tokio::test]
    #[serial]
    async fn import_skills_with_symlink_imports_selected_skill_manifest_parent() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let source_dir = tmp.path().join("single-skill");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: selected-manifest\ndescription: Selected manifest skill\n---\nBody",
        )
        .unwrap();

        let names = import_skills_with_symlink(&paths, &source_dir.join(SKILL_MANIFEST_FILE))
            .await
            .unwrap();
        assert_eq!(names, vec!["selected-manifest"]);

        let link_path = paths.user_skills_dir.join("selected-manifest");
        assert!(link_path.is_symlink());
        assert_eq!(std::fs::read_link(&link_path).unwrap(), source_dir);
        assert!(link_path.join(SKILL_MANIFEST_FILE).exists());
    }

    #[tokio::test]
    #[serial]
    async fn import_skills_with_symlink_imports_parent_directory_children() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let source_dir = tmp.path().join("skill-pack");
        create_skill_in_dir(&source_dir, "alpha", "Alpha skill");
        create_skill_in_dir(&source_dir, "beta", "Beta skill");

        let names = import_skills_with_symlink(&paths, &source_dir)
            .await
            .unwrap();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert!(paths.user_skills_dir.join("alpha").is_symlink());
        assert!(paths.user_skills_dir.join("beta").is_symlink());
    }

    #[tokio::test]
    async fn import_skills_with_symlink_imports_nested_bundle() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        // Skills nested two levels deep: pack/category/<skill>/SKILL.md.
        let nested = tmp.path().join("pack").join("category");
        create_skill_in_dir(&nested, "deep-alpha", "Deep alpha skill");
        create_skill_in_dir(&nested, "deep-beta", "Deep beta skill");

        // Picking the grandparent "pack" only works with recursive scanning —
        // neither it nor its immediate child contains a SKILL.md.
        let names = import_skills_with_symlink(&paths, &tmp.path().join("pack"))
            .await
            .unwrap();
        assert_eq!(names, vec!["deep-alpha", "deep-beta"]);
        assert!(paths.user_skills_dir.join("deep-alpha").exists());
        assert!(paths.user_skills_dir.join("deep-beta").exists());
    }

    #[tokio::test]
    async fn import_skills_with_symlink_best_effort_skips_bad_skill() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let pack = tmp.path().join("mixed-pack");
        create_skill_in_dir(&pack, "good-skill", "A valid skill");
        // Malformed sibling: frontmatter present but no description -> rejected.
        let bad_dir = pack.join("bad-skill");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(
            bad_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: bad-skill\n---\nBody",
        )
        .unwrap();

        // The bad skill is skipped; the good one still imports (non-atomic,
        // best-effort) instead of aborting the whole request.
        let names = import_skills_with_symlink(&paths, &pack).await.unwrap();
        assert_eq!(names, vec!["good-skill"]);
        assert!(paths.user_skills_dir.join("good-skill").exists());
        assert!(!paths.user_skills_dir.join("bad-skill").exists());
    }

    // NOTE: the import->copy fallback (import_skill_with_symlink now routing
    // through link_skill_or_fallback_copy) is covered by
    // `link_workspace_skills_falls_back_to_copy_when_symlink_fails`; a second
    // test here would race on the global FORCE_SYMLINK_FAILURE flag under
    // `cargo test` (nextest isolates per-process), so it is intentionally omitted.

    #[tokio::test]
    async fn list_available_skills_orders_custom_skills_by_newest_import_first() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let older_dir = tmp.path().join("older-source");
        let newer_dir = tmp.path().join("newer-source");
        std::fs::create_dir_all(&older_dir).unwrap();
        std::fs::create_dir_all(&newer_dir).unwrap();
        std::fs::write(
            older_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: older-skill\ndescription: Older skill\n---\nBody",
        )
        .unwrap();
        std::fs::write(
            newer_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: newer-skill\ndescription: Newer skill\n---\nBody",
        )
        .unwrap();

        import_skill_with_symlink(&paths, &older_dir).await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        import_skill_with_symlink(&paths, &newer_dir).await.unwrap();

        let skills = list_available_skills(&paths).await.unwrap();
        let names: Vec<_> = skills.into_iter().map(|skill| skill.name).collect();
        assert_eq!(names[0], "newer-skill");
        assert_eq!(names[1], "older-skill");
    }

    #[tokio::test]
    async fn import_skills_with_symlink_imports_zip_package() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let zip_path = tmp.path().join("skills.zip");

        write_test_zip(
            &zip_path,
            &[
                (
                    "bundle/zip-one/SKILL.md",
                    "---\nname: zip-one\ndescription: First zipped skill\n---\nBody",
                ),
                ("bundle/zip-one/data.txt", "payload"),
                (
                    "bundle/zip-two/SKILL.md",
                    "---\nname: zip-two\ndescription: Second zipped skill\n---\nBody",
                ),
            ],
        );

        let names = import_skills_with_symlink(&paths, &zip_path).await.unwrap();
        assert_eq!(names, vec!["zip-one", "zip-two"]);
        assert!(
            paths
                .user_skills_dir
                .join("zip-one")
                .join(SKILL_MANIFEST_FILE)
                .exists()
        );
        assert!(
            paths
                .user_skills_dir
                .join("zip-one")
                .join("data.txt")
                .exists()
        );
        assert!(!paths.user_skills_dir.join("zip-one").is_symlink());
        assert!(
            !paths
                .user_skills_dir
                .join(".import-tmp")
                .join("skills.zip")
                .exists()
        );
    }

    #[tokio::test]
    async fn import_skills_with_symlink_rejects_zip_slip_entries() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());
        let zip_path = tmp.path().join("evil.zip");

        write_test_zip(&zip_path, &[("../escape.txt", "outside")]);

        let result = import_skills_with_symlink(&paths, &zip_path).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
        assert!(!tmp.path().join("escape.txt").exists());
    }

    #[tokio::test]
    async fn import_skill_rejects_traversal_name() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        // Create a skill whose frontmatter name contains path traversal
        let source_dir = tmp.path().join("evil-skill");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: ../../../etc/evil\ndescription: Malicious skill\n---\nBody",
        )
        .unwrap();

        let result = import_skill(&paths, &source_dir).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn import_skill_with_symlink_rejects_traversal_name() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let source_dir = tmp.path().join("evil-skill");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: ../../escape\ndescription: Malicious skill\n---\nBody",
        )
        .unwrap();

        let result = import_skill_with_symlink(&paths, &source_dir).await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn delete_custom_skill() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        create_skill_in_dir(&paths.user_skills_dir, "to-delete", "Will be deleted");

        delete_skill(&paths, "to-delete").await.unwrap();
        assert!(!paths.user_skills_dir.join("to-delete").exists());
    }

    #[tokio::test]
    async fn delete_builtin_skill_rejected() {
        let tmp = TempDir::new().unwrap();
        let paths = make_disk_builtin_paths(tmp.path());
        let builtin_dir = disk_builtin_dir(&paths).to_path_buf();

        create_skill_in_dir(&builtin_dir, "protected", "Built-in skill");

        let result = delete_skill(&paths, "protected").await;
        assert!(matches!(
            result,
            Err(ExtensionError::BuiltinSkillDeletion(_))
        ));
    }

    #[tokio::test]
    async fn delete_nonexistent_skill() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let result = delete_skill(&paths, "ghost").await;
        assert!(matches!(result, Err(ExtensionError::SkillNotFound(_))));
    }

    #[tokio::test]
    async fn delete_skill_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let paths = make_test_paths(tmp.path());

        let result = delete_skill(&paths, "../etc").await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    // -----------------------------------------------------------------------
    // Scanning
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn scan_for_skills_finds_valid() {
        let tmp = TempDir::new().unwrap();
        create_skill_in_dir(tmp.path(), "skill-a", "First skill");
        create_skill_in_dir(tmp.path(), "skill-b", "Second skill");

        // Create a dir without SKILL.md (should be ignored)
        std::fs::create_dir_all(tmp.path().join("not-a-skill")).unwrap();

        let skills = scan_for_skills(tmp.path()).await.unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "skill-a");
        assert_eq!(skills[1].name, "skill-b");
    }

    #[tokio::test]
    async fn scan_for_skills_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let skills = scan_for_skills(tmp.path()).await.unwrap();
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn scan_for_skills_nonexistent_dir() {
        let skills = scan_for_skills(Path::new("/nonexistent/path"))
            .await
            .unwrap();
        assert!(skills.is_empty());
    }

    // -----------------------------------------------------------------------
    // Export
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn export_skill_creates_symlink() {
        let tmp = TempDir::new().unwrap();
        let source_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join(SKILL_MANIFEST_FILE),
            "---\nname: my-skill\ndescription: Test\n---\nBody",
        )
        .unwrap();

        let target_dir = tmp.path().join("exports");
        export_skill_with_symlink(&source_dir, &target_dir)
            .await
            .unwrap();

        let link = target_dir.join("my-skill");
        assert!(link.is_symlink());
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_test_paths(base: &Path) -> SkillPaths {
        // Hand out an empty on-disk builtin-skills dir. Tests that need
        // specific fixtures seed it via `create_skill_in_dir`; tests
        // that want the full real corpus use `make_embedded_paths`.
        SkillPaths {
            data_dir: base.to_path_buf(),
            user_skills_dir: base.join(SKILLS_DIR_NAME),
            cron_skills_dir: base.join(CRON_SKILLS_DIR_NAME),
            builtin_skills_dir: base.join(crate::constants::BUILTIN_SKILLS_DIR_NAME),
            builtin_rules_dir: base.join(BUILTIN_RULES_DIR_NAME),
            assistant_rules_dir: base.join(ASSISTANT_RULES_DIR_NAME),
            assistant_skills_dir: base.join(ASSISTANT_SKILLS_DIR_NAME),
        }
    }

    /// Return `SkillPaths` pre-populated with the real embedded builtin
    /// skills corpus materialized to disk. Use this for tests that
    /// previously relied on the embedded-corpus fallback.
    async fn make_embedded_paths(base: &Path) -> SkillPaths {
        crate::startup_materialize::materialize_embedded_builtin_skills(
            base,
            &BUILTIN_SKILLS,
            "test-version",
        )
        .await
        .expect("failed to materialize embedded corpus for test");
        make_test_paths(base)
    }

    /// Return a `SkillPaths` rooted at `base` with an on-disk
    /// `builtin_skills_dir`, so tests can seed fixtures in that dir.
    fn make_disk_builtin_paths(base: &Path) -> SkillPaths {
        make_test_paths(base)
    }

    fn disk_builtin_dir(paths: &SkillPaths) -> &Path {
        &paths.builtin_skills_dir
    }

    fn create_skill_in_dir(base: &Path, name: &str, description: &str) {
        let dir = base.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(SKILL_MANIFEST_FILE),
            format!("---\nname: {name}\ndescription: {description}\n---\nBody content for {name}."),
        )
        .unwrap();
    }

    fn write_test_zip(path: &Path, entries: &[(&str, &str)]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();

        for (name, content) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }

        zip.finish().unwrap();
    }

    // -----------------------------------------------------------------------
    // Embedded corpus
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn embedded_lists_auto_inject_from_corpus() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let autos = list_builtin_auto_skills(&paths).await.unwrap();
        assert!(
            autos.len() >= 4,
            "expected ≥4 auto-inject entries, got {}",
            autos.len()
        );
        for item in &autos {
            assert!(
                item.location.starts_with("auto-inject/"),
                "location must start with auto-inject/, got {}",
                item.location
            );
            assert!(item.location.ends_with("/SKILL.md"));
            assert!(!item.description.is_empty());
        }
    }

    #[tokio::test]
    async fn embedded_reads_builtin_skill_content() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let content = read_builtin_skill(&paths, "auto-inject/cron/SKILL.md")
            .await
            .unwrap();
        assert!(!content.is_empty(), "embedded cron SKILL.md is empty");
        assert!(
            content.trim_start().starts_with("---"),
            "expected frontmatter, got: {}",
            content.chars().take(80).collect::<String>()
        );
    }

    #[tokio::test]
    async fn embedded_rejects_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let result = read_builtin_skill(&paths, "../etc/passwd").await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));

        let result = read_builtin_skill(&paths, "auto-inject/../../secret").await;
        assert!(matches!(result, Err(ExtensionError::PathTraversal(_))));
    }

    #[tokio::test]
    async fn embedded_handles_missing_file() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let content = read_builtin_skill(&paths, "nonexistent/SKILL.md")
            .await
            .unwrap();
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn disk_override_reads_from_disk_not_embedded() {
        let tmp = TempDir::new().unwrap();
        let paths = make_disk_builtin_paths(tmp.path());
        let builtin_dir = disk_builtin_dir(&paths).to_path_buf();
        let auto_dir = builtin_dir.join(BUILTIN_AUTO_SKILLS_SUBDIR);
        create_skill_in_dir(&auto_dir, "fixture-only", "Fixture-only skill");

        let autos = list_builtin_auto_skills(&paths).await.unwrap();
        let names: Vec<&str> = autos.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"fixture-only"),
            "disk override should reflect seeded skill; got {names:?}"
        );
        // Embedded skills (e.g. `cron`) must NOT leak into the disk view.
        assert!(
            !names.contains(&"cron"),
            "disk override must not include embedded skills"
        );
    }

    #[tokio::test]
    async fn list_skills_builtin_has_relative_location_from_embedded() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let skills = list_available_skills(&paths).await.unwrap();
        let builtins: Vec<_> = skills
            .iter()
            .filter(|s| s.source == SkillSource::Builtin)
            .collect();
        assert!(!builtins.is_empty(), "no builtin skills listed");
        for s in &builtins {
            let rel = s
                .relative_location
                .as_deref()
                .expect("builtin must have relative_location");
            assert!(
                rel.ends_with("/SKILL.md"),
                "relative_location must end in /SKILL.md, got {rel}"
            );
            assert!(
                s.location
                    .contains(crate::constants::BUILTIN_SKILLS_DIR_NAME),
                "builtin location must live under the view dir, got {}",
                s.location
            );
            // Lazy materialization wrote SKILL.md to disk.
            assert!(
                std::path::Path::new(&s.location).exists(),
                "materialized view missing: {}",
                s.location
            );
        }
    }

    // -----------------------------------------------------------------------
    // Materialize (symlink contract)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn materialize_empty_list_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let list = materialize_skills_for_agent(&paths, "conv-empty", &[])
            .await
            .unwrap();
        assert!(list.is_empty());
        // No per-conversation dir should be created.
        assert!(!paths.data_dir.join("agent-skills").exists());
        assert!(!paths.data_dir.join("conversations").exists());
    }

    #[tokio::test]
    async fn materialize_resolves_auto_inject_skill_by_name() {
        // Auto-inject skills are resolved only when the caller names
        // them explicitly (see `ConversationService::create` snapshot).
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let resolved = materialize_skills_for_agent(&paths, "conv-named", &["cron".to_owned()])
            .await
            .unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "cron");
        // source_path points at the real on-disk auto-inject directory.
        let expected = paths
            .builtin_skills_dir
            .join(BUILTIN_AUTO_SKILLS_SUBDIR)
            .join("cron");
        assert_eq!(resolved[0].source_path, expected);
        assert!(resolved[0].source_path.is_dir());
        assert!(resolved[0].source_path.join(SKILL_MANIFEST_FILE).exists());
    }

    #[tokio::test]
    async fn materialize_resolves_opt_in_top_level_skill() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let resolved = materialize_skills_for_agent(&paths, "conv-opt", &["mermaid".to_owned()])
            .await
            .unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "mermaid");
        let expected = paths.builtin_skills_dir.join("mermaid");
        assert_eq!(resolved[0].source_path, expected);
    }

    #[tokio::test]
    async fn materialize_resolves_user_skill() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;
        create_skill_in_dir(&paths.user_skills_dir, "my-custom", "A user skill");

        let resolved = materialize_skills_for_agent(&paths, "conv-user", &["my-custom".to_owned()])
            .await
            .unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0].source_path,
            paths.user_skills_dir.join("my-custom")
        );
    }

    #[tokio::test]
    async fn materialize_silently_skips_unknown_skill() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let resolved =
            materialize_skills_for_agent(&paths, "conv-missing", &["no-such-skill".to_owned()])
                .await
                .unwrap();
        assert!(resolved.is_empty());
    }

    #[tokio::test]
    async fn materialize_skips_invalid_names_but_keeps_valid_ones() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let resolved = materialize_skills_for_agent(
            &paths,
            "conv-mixed",
            &[
                "".to_owned(),
                "../evil".to_owned(),
                "foo/bar".to_owned(),
                "cron".to_owned(),
            ],
        )
        .await
        .unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "cron");
    }

    #[tokio::test]
    async fn materialize_returns_sorted_list_with_source_paths() {
        // Deterministic ordering — callers rely on it for stable symlink
        // layouts and for easier debugging / snapshot tests.
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let resolved = materialize_skills_for_agent(
            &paths,
            "conv-sorted",
            &["mermaid".to_owned(), "cron".to_owned()],
        )
        .await
        .unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].name, "cron");
        assert_eq!(resolved[1].name, "mermaid");
        for entry in &resolved {
            assert!(entry.source_path.is_absolute());
            assert!(entry.source_path.is_dir());
        }
    }

    #[tokio::test]
    async fn materialize_rejects_bad_conversation_id() {
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let err = materialize_skills_for_agent(&paths, "../evil", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, ExtensionError::PathTraversal(_)));
    }

    #[tokio::test]
    async fn materialize_does_not_touch_disk_beyond_reads() {
        // Guardrail: the symlink contract forbids any per-conversation
        // directory on disk. Verify the function only reads the sources
        // and never writes.
        let tmp = TempDir::new().unwrap();
        let paths = make_embedded_paths(tmp.path()).await;

        let _ = materialize_skills_for_agent(&paths, "conv-pure", &["cron".to_owned()])
            .await
            .unwrap();
        assert!(!paths.data_dir.join("agent-skills").exists());
        assert!(!paths.data_dir.join("conversations").exists());
    }

    // -----------------------------------------------------------------------
    // Windows symlink → copy_dir_recursive fallback
    // -----------------------------------------------------------------------

    /// When the platform symlink primitive fails (mirrors Windows
    /// `os error 1314 ERROR_PRIVILEGE_NOT_HELD`), `link_workspace_skills`
    /// must materialize the skill via `copy_dir_recursive` instead so the
    /// CLI agent can still discover it. Forced via `ForceFailureGuard`
    /// on Linux/macOS CI where symlinking would otherwise succeed.
    #[tokio::test]
    #[serial]
    async fn link_workspace_skills_falls_back_to_copy_when_symlink_fails() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let source_root = tmp.path().join("sources");

        // Seed a fake skill source directory with a SKILL.md and a
        // nested file so we can verify the copy is recursive.
        let skill_source = source_root.join("my-skill");
        std::fs::create_dir_all(skill_source.join("nested")).unwrap();
        std::fs::write(
            skill_source.join(SKILL_MANIFEST_FILE),
            "---\nname: my-skill\ndescription: test\n---\nbody",
        )
        .unwrap();
        std::fs::write(skill_source.join("nested").join("data.txt"), "payload").unwrap();

        let resolved = vec![ResolvedAgentSkill {
            name: "my-skill".to_owned(),
            source_path: skill_source.clone(),
        }];

        // Force the symlink primitive to fail for the duration of this
        // test, exercising the copy fallback branch.
        let _guard = test_overrides::ForceFailureGuard::new();

        let created = link_workspace_skills(&workspace, &[".claude/skills"], &resolved)
            .await
            .expect("link_workspace_skills should succeed via copy fallback");
        assert_eq!(created, 1, "exactly one skill should be materialized");

        let target = workspace.join(".claude/skills").join("my-skill");
        assert!(target.exists(), "target directory must exist");
        // It must NOT be a symlink — fallback path uses copy_dir_recursive.
        let meta = tokio::fs::symlink_metadata(&target).await.unwrap();
        assert!(
            !meta.file_type().is_symlink(),
            "fallback must produce a real directory, not a symlink"
        );
        assert!(target.is_dir(), "target must be a directory");

        // Verify the contents were copied recursively.
        let manifest = std::fs::read_to_string(target.join(SKILL_MANIFEST_FILE)).unwrap();
        assert!(manifest.contains("name: my-skill"));
        let nested = std::fs::read_to_string(target.join("nested").join("data.txt")).unwrap();
        assert_eq!(nested, "payload");
    }

    /// Windows-only: directory linking must go through an NTFS junction
    /// (created by the `junction` crate) rather than `symlink_dir`, so
    /// the link works for users without Developer Mode. We assert the
    /// resulting path is a reparse point (junction is reported as a
    /// symlink by `symlink_metadata().file_type().is_symlink()`) and
    /// that the source contents are reachable through the link.
    ///
    /// The test is skipped on non-Windows platforms.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    #[serial]
    async fn link_workspace_skills_uses_junction_on_windows() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let source_root = tmp.path().join("sources");

        let skill_source = source_root.join("my-skill");
        std::fs::create_dir_all(skill_source.join("nested")).unwrap();
        std::fs::write(
            skill_source.join(SKILL_MANIFEST_FILE),
            "---\nname: my-skill\ndescription: test\n---\nbody",
        )
        .unwrap();
        std::fs::write(skill_source.join("nested").join("data.txt"), "payload").unwrap();

        let resolved = vec![ResolvedAgentSkill {
            name: "my-skill".to_owned(),
            source_path: skill_source.clone(),
        }];

        let created = link_workspace_skills(&workspace, &[".claude/skills"], &resolved)
            .await
            .expect("link_workspace_skills should succeed via junction");
        assert_eq!(created, 1, "exactly one skill should be materialized");

        let target = workspace.join(".claude/skills").join("my-skill");
        assert!(target.exists(), "target path must exist");

        // Junctions are reparse points; `symlink_metadata` reports them
        // as symlinks on Windows. The directory copy fallback would
        // produce a real directory (is_symlink() == false).
        let meta = std::fs::symlink_metadata(&target).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "Windows directory link must be a junction (reparse point), \
             not a copied directory"
        );

        // Reading through the link must surface the source contents.
        let manifest = std::fs::read_to_string(target.join(SKILL_MANIFEST_FILE)).unwrap();
        assert!(manifest.contains("name: my-skill"));
        let nested = std::fs::read_to_string(target.join("nested").join("data.txt")).unwrap();
        assert_eq!(nested, "payload");
    }
}

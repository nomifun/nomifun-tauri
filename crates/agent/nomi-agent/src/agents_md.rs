use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use nomi_config::config::{
    ProjectInstructionsConfig, app_config_dir,
};

const INSTRUCTION_PREAMBLE: &str = "Codebase and user instructions are shown below. \
Be sure to adhere to these instructions. IMPORTANT: These instructions OVERRIDE any \
default behavior and you MUST follow them exactly as written.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsMdFile {
    pub path: PathBuf,
    pub content: String,
    pub is_global: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsMdDiagnostic {
    path: Option<PathBuf>,
    detail: String,
}

impl AgentsMdDiagnostic {
    fn for_path(path: impl Into<PathBuf>, detail: impl Into<String>) -> Self {
        Self {
            path: Some(path.into()),
            detail: detail.into(),
        }
    }

    fn for_config(detail: impl Into<String>) -> Self {
        Self {
            path: None,
            detail: detail.into(),
        }
    }

    pub fn message(&self) -> String {
        match &self.path {
            Some(path) => format!("{}: {}", path.display(), self.detail),
            None => self.detail.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsMdSnapshot {
    pub project_root: Option<PathBuf>,
    pub files: Vec<AgentsMdFile>,
    pub formatted: String,
    pub project_bytes: usize,
    pub truncated: bool,
    pub diagnostics: Vec<AgentsMdDiagnostic>,
}

struct SelectedInstruction {
    path: PathBuf,
    content: String,
}

/// Resolve the immutable user/project instruction snapshot for a session.
pub fn resolve_agents_md(
    cwd: &Path,
    config: &ProjectInstructionsConfig,
) -> AgentsMdSnapshot {
    let user_dir = app_config_dir();
    resolve_agents_md_from(cwd, config, user_dir.as_deref())
}

pub fn format_agents_md_section(files: &[AgentsMdFile]) -> String {
    if files.is_empty() {
        return String::new();
    }

    let mut parts = vec![INSTRUCTION_PREAMBLE.to_owned()];
    for file in files {
        let description = if file.is_global {
            "(user's global instructions for all projects)"
        } else {
            "(project instructions)"
        };
        parts.push(format!(
            "Contents of {} {description}:\n\n{}",
            file.path.display(),
            file.content.trim()
        ));
    }
    parts.join("\n\n")
}

fn resolve_agents_md_from(
    cwd: &Path,
    config: &ProjectInstructionsConfig,
    user_dir: Option<&Path>,
) -> AgentsMdSnapshot {
    let cwd = normalize_path(cwd);
    let project_root = detect_project_root(&cwd, &config.project_root_markers);
    let mut files = Vec::new();
    let mut diagnostics = Vec::new();

    if let Some(user_dir) = user_dir {
        let candidates = vec!["AGENTS.override.md".to_owned(), "AGENTS.md".to_owned()];
        if let Some(selected) = select_instruction_file(user_dir, &candidates, &mut diagnostics) {
            files.push(AgentsMdFile {
                path: selected.path,
                content: selected.content,
                is_global: true,
            });
        }
    }

    let project_candidates = project_candidate_names(config, &mut diagnostics);
    let mut remaining = config.project_doc_max_bytes;
    let mut project_bytes = 0;
    let mut truncated = false;

    for directory in directory_chain(&project_root, &cwd) {
        let Some(selected) =
            select_instruction_file(&directory, &project_candidates, &mut diagnostics)
        else {
            continue;
        };

        if selected.content.len() <= remaining {
            project_bytes += selected.content.len();
            remaining -= selected.content.len();
            files.push(AgentsMdFile {
                path: selected.path,
                content: selected.content,
                is_global: false,
            });
            if remaining == 0 {
                break;
            }
            continue;
        }

        let prefix = utf8_prefix(&selected.content, remaining);
        project_bytes += prefix.len();
        if !prefix.is_empty() {
            files.push(AgentsMdFile {
                path: selected.path.clone(),
                content: prefix.to_owned(),
                is_global: false,
            });
        }
        truncated = true;
        diagnostics.push(AgentsMdDiagnostic::for_path(
            selected.path,
            format!(
                "project instructions truncated at the configured {} byte limit",
                config.project_doc_max_bytes
            ),
        ));
        break;
    }

    let formatted = format_agents_md_section(&files);
    AgentsMdSnapshot {
        project_root: Some(project_root),
        files,
        formatted,
        project_bytes,
        truncated,
        diagnostics,
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn detect_project_root(cwd: &Path, markers: &[String]) -> PathBuf {
    if markers.is_empty() {
        return cwd.to_path_buf();
    }

    let valid_markers: Vec<&str> = markers
        .iter()
        .map(String::as_str)
        .filter(|marker| safe_relative_name(marker))
        .collect();
    if valid_markers.is_empty() {
        return cwd.to_path_buf();
    }

    let mut current = cwd.to_path_buf();
    loop {
        if valid_markers
            .iter()
            .any(|marker| current.join(marker).exists())
        {
            return current;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return cwd.to_path_buf(),
        }
    }
}

fn directory_chain(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        directories.push(current.clone());
        if current == root {
            break;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return vec![cwd.to_path_buf()],
        }
    }
    directories.reverse();
    directories
}

fn project_candidate_names(
    config: &ProjectInstructionsConfig,
    diagnostics: &mut Vec<AgentsMdDiagnostic>,
) -> Vec<String> {
    let mut candidates = vec!["AGENTS.override.md".to_owned(), "AGENTS.md".to_owned()];
    for fallback in &config.project_doc_fallback_filenames {
        if safe_relative_name(fallback) {
            candidates.push(fallback.clone());
        } else {
            diagnostics.push(AgentsMdDiagnostic::for_config(format!(
                "ignored unsafe project instruction fallback filename {fallback:?}"
            )));
        }
    }
    candidates
}

fn safe_relative_name(name: &str) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    let path = Path::new(name);
    if path.is_absolute() {
        return false;
    }
    let mut has_normal_component = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => has_normal_component = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    has_normal_component
}

fn select_instruction_file(
    directory: &Path,
    candidates: &[String],
    diagnostics: &mut Vec<AgentsMdDiagnostic>,
) -> Option<SelectedInstruction> {
    for candidate in candidates {
        let path = directory.join(candidate);
        match std::fs::read_to_string(&path) {
            Ok(content) if content.trim().is_empty() => continue,
            Ok(content) => return Some(SelectedInstruction { path, content }),
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => diagnostics.push(AgentsMdDiagnostic::for_path(
                path,
                format!("failed to read instruction file: {error}"),
            )),
        }
    }
    None
}

fn utf8_prefix(content: &str, max_bytes: usize) -> &str {
    let mut end = content.len().min(max_bytes);
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    &content[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomi_config::config::ProjectInstructionsConfig;
    use std::fs;
    use tempfile::TempDir;

    fn default_config() -> ProjectInstructionsConfig {
        ProjectInstructionsConfig::default()
    }

    #[test]
    fn nested_workspace_loads_project_root_before_leaf() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join("AGENTS.md"), "ROOT_RULE").unwrap();
        let leaf = root.join("crates").join("agent");
        fs::create_dir_all(&leaf).unwrap();
        fs::write(leaf.join("AGENTS.md"), "LEAF_RULE").unwrap();

        let snapshot = resolve_agents_md_from(&leaf, &default_config(), None);

        let canonical_root = root.canonicalize().unwrap();
        let canonical_leaf = leaf.canonicalize().unwrap();
        assert_eq!(snapshot.project_root.as_deref(), Some(canonical_root.as_path()));
        assert_eq!(snapshot.files.len(), 2);
        assert_eq!(snapshot.files[0].path, canonical_root.join("AGENTS.md"));
        assert_eq!(snapshot.files[1].path, canonical_leaf.join("AGENTS.md"));
        assert!(
            snapshot.formatted.find("ROOT_RULE").unwrap()
                < snapshot.formatted.find("LEAF_RULE").unwrap()
        );
    }

    #[test]
    fn override_wins_over_regular_and_fallback_in_the_same_directory() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::write(tmp.path().join("AGENTS.override.md"), "OVERRIDE_RULE").unwrap();
        fs::write(tmp.path().join("AGENTS.md"), "REGULAR_RULE").unwrap();
        fs::write(tmp.path().join("TEAM.md"), "FALLBACK_RULE").unwrap();
        let config = ProjectInstructionsConfig {
            project_doc_fallback_filenames: vec!["TEAM.md".into()],
            ..Default::default()
        };

        let snapshot = resolve_agents_md_from(tmp.path(), &config, None);

        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(
            snapshot.files[0].path,
            tmp.path().canonicalize().unwrap().join("AGENTS.override.md")
        );
        assert!(snapshot.formatted.contains("OVERRIDE_RULE"));
        assert!(!snapshot.formatted.contains("REGULAR_RULE"));
        assert!(!snapshot.formatted.contains("FALLBACK_RULE"));
    }

    #[test]
    fn empty_and_invalid_utf8_candidates_fall_through_in_priority_order() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::write(tmp.path().join("AGENTS.override.md"), "  \n").unwrap();
        fs::write(tmp.path().join("AGENTS.md"), [0xff, 0xfe]).unwrap();
        fs::write(tmp.path().join("TEAM.md"), "FALLBACK_RULE").unwrap();
        let config = ProjectInstructionsConfig {
            project_doc_fallback_filenames: vec!["TEAM.md".into()],
            ..Default::default()
        };

        let snapshot = resolve_agents_md_from(tmp.path(), &config, None);

        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(
            snapshot.files[0].path,
            tmp.path().canonicalize().unwrap().join("TEAM.md")
        );
        assert!(snapshot.formatted.contains("FALLBACK_RULE"));
        assert!(
            snapshot
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message().contains("AGENTS.md"))
        );
    }

    #[test]
    fn missing_root_marker_limits_project_scope_to_working_directory() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("AGENTS.md"), "PARENT_RULE").unwrap();
        let leaf = tmp.path().join("nested");
        fs::create_dir(&leaf).unwrap();
        fs::write(leaf.join("AGENTS.md"), "LEAF_RULE").unwrap();

        let snapshot = resolve_agents_md_from(&leaf, &default_config(), None);

        let canonical_leaf = leaf.canonicalize().unwrap();
        assert_eq!(snapshot.project_root.as_deref(), Some(canonical_leaf.as_path()));
        assert_eq!(snapshot.files.len(), 1);
        assert!(snapshot.formatted.contains("LEAF_RULE"));
        assert!(!snapshot.formatted.contains("PARENT_RULE"));
    }

    #[test]
    fn custom_and_empty_project_root_markers_match_codex() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".hg")).unwrap();
        fs::write(tmp.path().join("AGENTS.md"), "ROOT_RULE").unwrap();
        let leaf = tmp.path().join("nested");
        fs::create_dir(&leaf).unwrap();
        fs::write(leaf.join("AGENTS.md"), "LEAF_RULE").unwrap();

        let custom = ProjectInstructionsConfig {
            project_root_markers: vec![".hg".into()],
            ..Default::default()
        };
        let custom_snapshot = resolve_agents_md_from(&leaf, &custom, None);
        assert_eq!(custom_snapshot.files.len(), 2);

        let disabled = ProjectInstructionsConfig {
            project_root_markers: Vec::new(),
            ..Default::default()
        };
        let disabled_snapshot = resolve_agents_md_from(&leaf, &disabled, None);
        assert_eq!(disabled_snapshot.files.len(), 1);
        assert!(disabled_snapshot.formatted.contains("LEAF_RULE"));
        assert!(!disabled_snapshot.formatted.contains("ROOT_RULE"));
    }

    #[test]
    fn combined_budget_truncates_utf8_safely_and_skips_later_files() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::write(tmp.path().join("AGENTS.md"), "1234éROOT_TAIL").unwrap();
        let leaf = tmp.path().join("nested");
        fs::create_dir(&leaf).unwrap();
        fs::write(leaf.join("AGENTS.md"), "LEAF_RULE").unwrap();
        let config = ProjectInstructionsConfig {
            project_doc_max_bytes: 5,
            ..Default::default()
        };

        let snapshot = resolve_agents_md_from(&leaf, &config, None);

        assert!(snapshot.truncated);
        assert_eq!(snapshot.project_bytes, 4);
        assert_eq!(snapshot.files.len(), 1);
        assert!(snapshot.formatted.contains("1234"));
        assert!(!snapshot.formatted.contains('é'));
        assert!(!snapshot.formatted.contains("LEAF_RULE"));
    }

    #[test]
    fn user_override_is_independent_from_zero_project_budget() {
        let project = TempDir::new().unwrap();
        fs::create_dir(project.path().join(".git")).unwrap();
        fs::write(project.path().join("AGENTS.md"), "PROJECT_RULE").unwrap();
        let user = TempDir::new().unwrap();
        fs::write(user.path().join("AGENTS.override.md"), "USER_OVERRIDE").unwrap();
        fs::write(user.path().join("AGENTS.md"), "USER_REGULAR").unwrap();
        let config = ProjectInstructionsConfig {
            project_doc_max_bytes: 0,
            ..Default::default()
        };

        let snapshot = resolve_agents_md_from(project.path(), &config, Some(user.path()));

        assert_eq!(snapshot.files.len(), 1);
        assert!(snapshot.files[0].is_global);
        assert!(snapshot.formatted.contains("USER_OVERRIDE"));
        assert!(!snapshot.formatted.contains("USER_REGULAR"));
        assert!(!snapshot.formatted.contains("PROJECT_RULE"));
    }

    #[test]
    fn unsafe_fallback_names_are_rejected_but_safe_nested_names_work() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::create_dir(tmp.path().join(".agent")).unwrap();
        fs::write(tmp.path().join(".agent/TEAM.md"), "SAFE_FALLBACK").unwrap();
        let config = ProjectInstructionsConfig {
            project_doc_fallback_filenames: vec![
                "../outside.md".into(),
                "/absolute.md".into(),
                "".into(),
                ".agent/TEAM.md".into(),
            ],
            ..Default::default()
        };

        let snapshot = resolve_agents_md_from(tmp.path(), &config, None);

        assert_eq!(snapshot.files.len(), 1);
        assert!(snapshot.formatted.contains("SAFE_FALLBACK"));
        assert_eq!(snapshot.diagnostics.len(), 3);
    }

    #[test]
    fn include_syntax_remains_literal_for_codex_compatibility() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::write(tmp.path().join("AGENTS.md"), "@rules.md").unwrap();
        fs::write(tmp.path().join("rules.md"), "MUST_NOT_EXPAND").unwrap();

        let snapshot = resolve_agents_md_from(tmp.path(), &default_config(), None);

        assert!(snapshot.formatted.contains("@rules.md"));
        assert!(!snapshot.formatted.contains("MUST_NOT_EXPAND"));
    }
}

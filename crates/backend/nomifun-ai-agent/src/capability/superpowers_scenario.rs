//! Coding-scenario detection + the `using-superpowers` bootstrap injected into
//! coding sessions.
//!
//! superpowers skills are general software-development methodology (TDD,
//! systematic-debugging, brainstorming, …). They add value only when the
//! session is actually doing coding work AND the agent can edit/run code, so we
//! gate their activation on a cheap, low-false-positive signal: the workspace
//! looks like a software project (a recognized manifest or VCS root), or the
//! session is explicitly tagged `coding`. When the scenario matches, the
//! factory feeds the superpowers skills to the engine and appends
//! [`SUPERPOWERS_BOOTSTRAP`] to the system prompt so the skills auto-trigger.
//!
//! Design: `docs/superpowers/specs/2026-07-07-superpowers-integration-design.md`.

use std::path::{Path, PathBuf};

/// Signals used to decide whether a session is a coding scenario. Populated at
/// session-build time from the workspace, capability toggles, and any scenario
/// tags carried by the agent/companion.
#[derive(Debug, Default, Clone)]
pub struct ScenarioSignals {
    /// Session workspace directory, if any.
    pub workspace: Option<PathBuf>,
    /// Whether file-editing / shell tools (Write/Edit/Bash) are available. The
    /// methodology is pointless without them, so this is a prerequisite.
    pub file_tools_enabled: bool,
    /// Scenario tags carried by the agent/companion (e.g. `coding`).
    pub scenario_tags: Vec<String>,
}

/// Files / dirs whose presence in a workspace marks it as a software project.
/// Deliberately excludes `.nomi` (a nomifun artifact, not a code-project signal)
/// to avoid treating every nomifun-touched workspace as a coding scenario.
const PROJECT_MARKERS: &[&str] = &[
    ".git",
    "Cargo.toml",
    "package.json",
    "tsconfig.json",
    "pyproject.toml",
    "requirements.txt",
    "setup.py",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "Gemfile",
    "composer.json",
    "CMakeLists.txt",
    "Makefile",
];

/// True if `ws` contains a recognized project marker (a VCS root or build
/// manifest) — i.e. it looks like a software project worth applying coding
/// methodology to.
pub fn workspace_is_code_project(ws: &Path) -> bool {
    PROJECT_MARKERS.iter().any(|marker| ws.join(marker).exists())
}

/// Decide whether superpowers should be activated for this session.
///
/// Requires file tools (the methodology needs to edit/run code) AND either an
/// explicit `coding` scenario tag or a workspace that looks like a software
/// project. Deliberately conservative — a false negative just means "not
/// enhanced", while a false positive would turn casual chats into TDD
/// interrogations.
pub fn is_coding_scenario(sig: &ScenarioSignals) -> bool {
    if !sig.file_tools_enabled {
        return false;
    }
    if sig
        .scenario_tags
        .iter()
        .any(|t| t.trim().eq_ignore_ascii_case("coding"))
    {
        return true;
    }
    sig.workspace.as_deref().is_some_and(workspace_is_code_project)
}

/// The `using-superpowers` bootstrap, nomi-flavored. Appended to the system
/// prompt of coding sessions so the embedded superpowers skills auto-trigger at
/// the right moments — this is what makes the integration "real" rather than
/// dead files on disk (see the superpowers project docs on session-start
/// bootstrap loading).
pub const SUPERPOWERS_BOOTSTRAP: &str = "【Superpowers 编码增强】本次为编码场景，已为你装载 superpowers 方法论技能库（brainstorming / test-driven-development / systematic-debugging / writing-plans / verification-before-completion 等，见上方技能清单）。动手前请先检查是否有匹配的技能：只要有哪怕 1% 的可能适用，就用 `Skill` 工具加载并严格遵循它——例如新增功能或改动行为前先走 brainstorming 厘清设计，写实现前按 test-driven-development 先写会失败的测试，遇到 bug/异常先用 systematic-debugging 定位根因，声称完成/修好前用 verification-before-completion 跑验证拿证据。这些是流程纪律而非可选建议；但用户明确的指令永远优先。";

/// Append [`SUPERPOWERS_BOOTSTRAP`] to a system prompt when the session is a
/// coding scenario (composition, not replacement — any persona/preset/knowledge
/// content is preserved before it). `active == false` returns `base` unchanged.
/// Mirrors `factory::nomi::compose_subagent_hint`; pure so it is unit-testable.
pub fn compose_superpowers_bootstrap(base: Option<String>, active: bool) -> Option<String> {
    if !active {
        return base;
    }
    Some(match base {
        Some(existing) if !existing.trim().is_empty() => format!("{existing}\n\n{SUPERPOWERS_BOOTSTRAP}"),
        _ => SUPERPOWERS_BOOTSTRAP.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn file_tools_are_a_prerequisite() {
        let sig = ScenarioSignals {
            workspace: None,
            file_tools_enabled: false,
            scenario_tags: vec!["coding".into()],
        };
        assert!(
            !is_coding_scenario(&sig),
            "no file tools → not a coding scenario even with a coding tag"
        );
    }

    #[test]
    fn coding_tag_triggers_when_tools_present() {
        let sig = ScenarioSignals {
            workspace: None,
            file_tools_enabled: true,
            scenario_tags: vec!["Coding".into()], // case-insensitive
        };
        assert!(is_coding_scenario(&sig));
    }

    #[test]
    fn code_project_workspace_triggers() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), b"[package]\n").unwrap();
        let sig = ScenarioSignals {
            workspace: Some(tmp.path().to_path_buf()),
            file_tools_enabled: true,
            scenario_tags: vec![],
        };
        assert!(is_coding_scenario(&sig));
    }

    #[test]
    fn plain_workspace_without_markers_is_not_coding() {
        let tmp = TempDir::new().unwrap();
        let sig = ScenarioSignals {
            workspace: Some(tmp.path().to_path_buf()),
            file_tools_enabled: true,
            scenario_tags: vec![],
        };
        assert!(!is_coding_scenario(&sig), "empty dir with no markers is not a code project");
    }

    #[test]
    fn workspace_markers_are_detected() {
        let tmp = TempDir::new().unwrap();
        assert!(!workspace_is_code_project(tmp.path()));
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        assert!(workspace_is_code_project(tmp.path()));
    }

    #[test]
    fn bootstrap_names_key_skills_and_the_skill_tool() {
        assert!(SUPERPOWERS_BOOTSTRAP.contains("brainstorming"));
        assert!(SUPERPOWERS_BOOTSTRAP.contains("test-driven-development"));
        assert!(SUPERPOWERS_BOOTSTRAP.contains("systematic-debugging"));
        assert!(SUPERPOWERS_BOOTSTRAP.contains("Skill"));
    }

    #[test]
    fn compose_appends_bootstrap_after_base_when_active() {
        let out = compose_superpowers_bootstrap(Some("人格提示".into()), true).unwrap();
        assert!(out.starts_with("人格提示"), "existing prompt preserved first");
        assert!(out.contains(SUPERPOWERS_BOOTSTRAP), "bootstrap appended");
    }

    #[test]
    fn compose_is_passthrough_when_inactive() {
        let base = Some("人格提示".to_string());
        assert_eq!(compose_superpowers_bootstrap(base.clone(), false), base);
        assert_eq!(compose_superpowers_bootstrap(None, false), None);
    }

    #[test]
    fn compose_uses_bootstrap_alone_when_base_empty() {
        assert_eq!(
            compose_superpowers_bootstrap(None, true),
            Some(SUPERPOWERS_BOOTSTRAP.to_owned())
        );
        assert_eq!(
            compose_superpowers_bootstrap(Some("   ".into()), true),
            Some(SUPERPOWERS_BOOTSTRAP.to_owned())
        );
    }
}

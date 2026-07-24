//! Integration tests for the skill system.
//!
//! These tests verify the full skill lifecycle:
//! - Skill discovery across multiple directories
//! - Skill index generation
//! - Lazy loading of skill bodies
//! - LOAD_SKILL detection in agent output
//! - System instruction building
//! - First message preparation

// Pre-existing: ENV_MUTEX MutexGuard held across await points is intentional —
// it serializes env-var mutation across tests.
#![allow(clippy::await_holding_lock)]

use std::fs;
use std::sync::{Arc, Mutex};

use nomifun_ai_agent::{
    AcpSkillManager, build_skills_index_text, build_system_instructions, detect_skill_load_request,
    prepare_first_message, prepare_first_message_with_skills_index,
};
use nomifun_extension::{BUILTIN_SKILLS_ENV_VAR, resolve_skill_paths};
use tempfile::TempDir;
/// Serialize env var mutations across tests — `BUILTIN_SKILLS_ENV_VAR` is
/// process-global so concurrent tests that set it must not interleave.
static ENV_MUTEX: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// 4.0 New API: discover via extension service
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discover_skills_uses_extension_service_layout() {
    let _guard = ENV_MUTEX.lock().unwrap();
    let tmp = TempDir::new().unwrap();
    let builtin_src = tmp.path().join("builtin-skills-src");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // auto-inject skill: builtin-src/auto-inject/cron/SKILL.md
    let auto_dir = builtin_src.join("auto-inject").join("cron");
    fs::create_dir_all(&auto_dir).unwrap();
    fs::write(
        auto_dir.join("SKILL.md"),
        "---\nname: cron\ndescription: Cron helper\n---\nBody",
    )
    .unwrap();

    // opt-in builtin: builtin-src/diagram-helper/SKILL.md
    let opt_dir = builtin_src.join("diagram-helper");
    fs::create_dir_all(&opt_dir).unwrap();
    fs::write(
        opt_dir.join("SKILL.md"),
        "---\nname: diagram-helper\ndescription: Diagram helper\n---\nBody",
    )
    .unwrap();

    // user custom: data/skills/my-skill/SKILL.md
    let user_dir = data_dir.join("skills").join("my-skill");
    fs::create_dir_all(&user_dir).unwrap();
    fs::write(
        user_dir.join("SKILL.md"),
        "---\nname: my-skill\ndescription: User skill\n---\nBody",
    )
    .unwrap();

    unsafe {
        std::env::set_var(BUILTIN_SKILLS_ENV_VAR, &builtin_src);
    }

    let paths = Arc::new(resolve_skill_paths(tmp.path(), &data_dir));
    let mgr = AcpSkillManager::new(paths);

    // No enabled_skills: opt-in builtin (diagram-helper) and custom
    // (my-skill) should be skipped. Only the auto-inject builtin (cron)
    // appears.
    let idx = mgr.discover_skills(None, None).await;
    let names: std::collections::HashSet<&str> = idx.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains("cron"), "auto-inject skill missing: got {names:?}");
    assert!(
        !names.contains("diagram-helper"),
        "opt-in builtin leaked without enabled_skills"
    );
    assert!(!names.contains("my-skill"), "custom leaked without enabled_skills");

    unsafe {
        std::env::remove_var(BUILTIN_SKILLS_ENV_VAR);
    }
}

#[tokio::test]
async fn get_skill_loads_builtin_body_via_read_builtin_skill() {
    let _guard = ENV_MUTEX.lock().unwrap();
    let tmp = TempDir::new().unwrap();
    let builtin = tmp.path().join("builtin");
    let auto = builtin.join("auto-inject").join("bodyskill");
    fs::create_dir_all(&auto).unwrap();
    fs::write(
        auto.join("SKILL.md"),
        "---\nname: bodyskill\ndescription: B\n---\nBuiltin body content",
    )
    .unwrap();

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    unsafe {
        std::env::set_var(BUILTIN_SKILLS_ENV_VAR, &builtin);
    }

    let paths = Arc::new(resolve_skill_paths(tmp.path(), &data_dir));
    let mgr = AcpSkillManager::new(paths);
    mgr.discover_skills(None, None).await;

    let skill = mgr.get_skill("bodyskill").await.unwrap();
    assert_eq!(
        skill.body.as_deref(),
        Some("Builtin body content"),
        "builtin body should be loaded via read_builtin_skill + extract_body"
    );

    unsafe {
        std::env::remove_var(BUILTIN_SKILLS_ENV_VAR);
    }
}

#[tokio::test]
async fn get_skill_loads_custom_body_via_fs_read() {
    let _guard = ENV_MUTEX.lock().unwrap();
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let user_skill = data_dir.join("skills").join("mine");
    fs::create_dir_all(&user_skill).unwrap();
    fs::write(
        user_skill.join("SKILL.md"),
        "---\nname: mine\ndescription: Mine\n---\nCustom body here",
    )
    .unwrap();

    // Ensure no stale BUILTIN_SKILLS_ENV_VAR interferes
    unsafe {
        std::env::remove_var(BUILTIN_SKILLS_ENV_VAR);
    }

    let paths = Arc::new(resolve_skill_paths(tmp.path(), &data_dir));
    let mgr = AcpSkillManager::new(paths);
    let enabled = vec!["mine".to_string()];
    let idx = mgr.discover_skills(Some(&enabled), None).await;

    let names: Vec<&str> = idx.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"mine"),
        "custom 'mine' should be in index; got {names:?}"
    );

    let skill = mgr.get_skill("mine").await.unwrap();
    assert_eq!(skill.body.as_deref(), Some("Custom body here"));
}

#[tokio::test]
async fn discover_skills_respects_exclude_builtin() {
    let _guard = ENV_MUTEX.lock().unwrap();
    let tmp = TempDir::new().unwrap();
    let builtin_src = tmp.path().join("b");
    let auto_dir = builtin_src.join("auto-inject").join("cron");
    fs::create_dir_all(&auto_dir).unwrap();
    fs::write(
        auto_dir.join("SKILL.md"),
        "---\nname: cron\ndescription: Cron\n---\nBody",
    )
    .unwrap();

    unsafe {
        std::env::set_var(BUILTIN_SKILLS_ENV_VAR, &builtin_src);
    }

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let paths = Arc::new(resolve_skill_paths(tmp.path(), &data_dir));
    let mgr = AcpSkillManager::new(paths);
    let exclude = vec!["cron".to_string()];
    let idx = mgr.discover_skills(None, Some(&exclude)).await;
    assert!(idx.is_empty(), "excluded auto-inject skill should be dropped");

    unsafe {
        std::env::remove_var(BUILTIN_SKILLS_ENV_VAR);
    }
}

// ---------------------------------------------------------------------------
// 5.2 Skill Index (pure function)
// ---------------------------------------------------------------------------

#[test]
fn build_index_text_contains_load_protocol() {
    let skills = vec![
        nomifun_ai_agent::SkillIndex {
            name: "security".into(),
            description: "Security review".into(),
        },
        nomifun_ai_agent::SkillIndex {
            name: "tdd".into(),
            description: "Test-driven development".into(),
        },
    ];
    let text = build_skills_index_text(&skills);

    assert!(text.contains("[LOAD_SKILL: skill-name]"));
    assert!(text.contains("- **security**: Security review"));
    assert!(text.contains("- **tdd**: Test-driven development"));
}

// ---------------------------------------------------------------------------
// 5.4 LOAD_SKILL Detection (pure function)
// ---------------------------------------------------------------------------

#[test]
fn detect_single_load_skill_request() {
    let content = "I need to use [LOAD_SKILL: security-review] to check this code.";
    let skills = detect_skill_load_request(content);
    assert_eq!(skills, vec!["security-review"]);
}

#[test]
fn detect_multiple_load_skill_requests() {
    let content = "[LOAD_SKILL: a] then [LOAD_SKILL: b] and [LOAD_SKILL: c]";
    let skills = detect_skill_load_request(content);
    assert_eq!(skills, vec!["a", "b", "c"]);
}

#[test]
fn detect_no_load_skill_in_normal_text() {
    let content = "This is just normal text without any skill requests.";
    let skills = detect_skill_load_request(content);
    assert!(skills.is_empty());
}

#[test]
fn detect_load_skill_handles_whitespace() {
    let content = "[LOAD_SKILL:   spaced-name   ]";
    let skills = detect_skill_load_request(content);
    assert_eq!(skills, vec!["spaced-name"]);
}

// ---------------------------------------------------------------------------
// System instruction and first message builders
// ---------------------------------------------------------------------------

#[test]
fn system_instructions_with_loaded_skills() {
    let skills = vec![nomifun_ai_agent::SkillDefinition {
        name: "helper".into(),
        description: "A helper".into(),
        location: std::path::PathBuf::new(),
        source: nomifun_extension::SkillSource::Custom,
        relative_location: None,
        body: Some("Complete helper instructions.".into()),
    }];
    let result = build_system_instructions("Base system prompt", &skills);

    assert!(result.starts_with("Base system prompt"));
    assert!(result.contains("## Skill: helper"));
    assert!(result.contains("Complete helper instructions."));
}

#[test]
fn first_message_with_skills_index_for_acp() {
    let skills = vec![nomifun_ai_agent::SkillIndex {
        name: "review".into(),
        description: "Code review".into(),
    }];
    let result = prepare_first_message_with_skills_index("Please review my code.", &skills, None);

    assert!(result.contains("[Assistant Rules]"));
    assert!(result.contains("- **review**: Code review"));
    assert!(result.contains("[/Assistant Rules]"));
    assert!(result.ends_with("Please review my code."));
}

#[test]
fn first_message_with_full_skills_for_gemini() {
    let skills = vec![nomifun_ai_agent::SkillDefinition {
        name: "debug".into(),
        description: "Debug".into(),
        location: std::path::PathBuf::new(),
        source: nomifun_extension::SkillSource::Custom,
        relative_location: None,
        body: Some("Full debug skill content.".into()),
    }];
    let result = prepare_first_message("Hello", &skills, Some("Be helpful."));

    assert!(result.contains("[Assistant Rules]"));
    assert!(result.contains("Be helpful."));
    assert!(result.contains("Full debug skill content."));
    assert!(result.contains("[/Assistant Rules]"));
    assert!(result.ends_with("Hello"));
}

// User-override tests moved to the BUILTIN_SKILLS_ENV_VAR-based discovery
// tests at the top of this file — see Task 5 for get_skill body-loading
// coverage against the new skill_service-backed API.

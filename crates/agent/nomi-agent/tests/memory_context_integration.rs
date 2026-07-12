// Integration tests for memory system context assembly (TC-7).
//
// These are black-box tests that verify the memory system is correctly
// integrated into the system prompt assembly pipeline.

use std::fs;

use nomi_agent::{
    agents_md::resolve_agents_md,
    context::{SystemPromptCache, build_system_prompt},
};
use nomi_config::config::ProjectInstructionsConfig;

// ---------------------------------------------------------------------------
// TC-7.1: With memory_dir, system prompt includes memory content
// ---------------------------------------------------------------------------

#[test]
fn tc_7_1_memory_dir_with_content_injects_prompt() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mem_dir = tmp.path().join("memory");
    fs::create_dir_all(&mem_dir).unwrap();
    fs::write(
        mem_dir.join("MEMORY.md"),
        "- [Role](user_role.md) \u{2014} senior engineer\n\
         - [Policy](feedback_tests.md) \u{2014} always use real DB\n",
    )
    .unwrap();

    let result = build_system_prompt(
        &mut SystemPromptCache::new(),
        None,
        "/tmp",
        "test-model",
        &[],
        None,
        Some(&mem_dir),
        false,
        false,
        false,
    );

    // Should contain minimal memory system sections
    assert!(
        result.contains("auto memory"),
        "should contain memory system display name"
    );
    assert!(
        result.contains("Memory types:"),
        "should contain compact type summary"
    );
    assert!(
        result.contains("MEMORY.md is the index"),
        "should contain compact save guidance"
    );

    // Should contain MEMORY.md content
    assert!(
        result.contains("user_role.md"),
        "should contain MEMORY.md entries"
    );
    assert!(
        result.contains("senior engineer"),
        "should contain entry descriptions"
    );
}

// ---------------------------------------------------------------------------
// TC-7.2: Without memory_dir, no memory injection
// ---------------------------------------------------------------------------

#[test]
fn tc_7_2_no_memory_dir_no_injection() {
    let result = build_system_prompt(
        &mut SystemPromptCache::new(),
        None,
        "/tmp",
        "test-model",
        &[],
        None,
        None,
        false,
        false,
        false,
    );

    assert!(
        !result.contains("auto memory"),
        "no memory content when memory_dir is None"
    );
    assert!(
        !result.contains("Types of memory"),
        "no type definitions when memory_dir is None"
    );
}

// ---------------------------------------------------------------------------
// TC-7.3: Memory appears after AGENTS.md, before skills
// ---------------------------------------------------------------------------

#[test]
fn tc_7_3_section_ordering() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cwd = tmp.path();

    // Create AGENTS.md
    fs::write(cwd.join("AGENTS.md"), "PROJECT_RULES_CONTENT").unwrap();

    // Create memory dir
    let mem_dir = tmp.path().join("memory");
    fs::create_dir_all(&mem_dir).unwrap();
    fs::write(mem_dir.join("MEMORY.md"), "- [A](a.md) \u{2014} test\n").unwrap();

    // Create a minimal skill metadata
    use nomi_skills::types::{ExecutionContext, LoadedFrom, SkillMetadata, SkillSource};
    let skill = SkillMetadata {
        name: "test-skill".to_string(),
        display_name: None,
        description: "A test skill".to_string(),
        has_user_specified_description: false,
        allowed_tools: vec![],
        argument_hint: None,
        argument_names: vec![],
        when_to_use: None,
        version: None,
        model: None,
        disable_model_invocation: false,
        user_invocable: true,
        execution_context: ExecutionContext::Inline,
        agent: None,
        effort: None,
        shell: None,
        paths: vec![],
        hooks_raw: None,
        source: SkillSource::User,
        loaded_from: LoadedFrom::Skills,
        content: String::new(),
        content_length: 0,
        skill_root: None,
    };

    // Runtime bootstrap resolves project instructions once and installs the
    // immutable snapshot into the session prompt cache.
    let snapshot = resolve_agents_md(cwd, &ProjectInstructionsConfig::default());
    let mut cache = SystemPromptCache::new();
    cache.set_agents_md(snapshot.formatted);
    let result = build_system_prompt(
        &mut cache,
        None,
        &cwd.to_string_lossy(),
        "test-model",
        &[skill],
        None,
        Some(&mem_dir),
        false,
        false,
        false,
    );

    let agents_pos = result
        .find("PROJECT_RULES_CONTENT")
        .expect("AGENTS.md content should be present");
    let memory_pos = result
        .find("auto memory")
        .expect("memory section should be present");
    let skills_pos = result
        .find("test-skill")
        .expect("skills section should be present");

    assert!(
        agents_pos < memory_pos,
        "AGENTS.md content should appear before memory section"
    );
    assert!(
        memory_pos < skills_pos,
        "memory section should appear before skills listing"
    );
}

// ---------------------------------------------------------------------------
// TC-7.4: Non-existent memory_dir degrades gracefully
// ---------------------------------------------------------------------------

#[test]
fn tc_7_4_nonexistent_dir_graceful_degradation() {
    let result = build_system_prompt(
        &mut SystemPromptCache::new(),
        None,
        "/tmp",
        "test-model",
        &[],
        None,
        Some(std::path::Path::new("/nonexistent/memory/dir")),
        false,
        false,
        false,
    );

    // Should not panic
    assert!(
        result.contains("currently empty"),
        "nonexistent memory dir should show empty state"
    );
    assert!(
        result.contains("auto memory"),
        "memory section should still be present (with empty state)"
    );
}

// ---------------------------------------------------------------------------
// TC-7.5: MEMORY.md content correctly injected
// ---------------------------------------------------------------------------

#[test]
fn tc_7_5_memory_md_content_injected() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mem_dir = tmp.path().join("memory");
    fs::create_dir_all(&mem_dir).unwrap();
    fs::write(
        mem_dir.join("MEMORY.md"),
        "- [User Role](user_role.md) \u{2014} senior engineer\n\
         - [Test Policy](feedback_tests.md) \u{2014} always use real DB\n\
         - [Sprint](project_sprint.md) \u{2014} sprint 42 ends Friday\n",
    )
    .unwrap();

    let result = build_system_prompt(
        &mut SystemPromptCache::new(),
        None,
        "/tmp",
        "test-model",
        &[],
        None,
        Some(&mem_dir),
        false,
        false,
        false,
    );

    assert!(
        result.contains("user_role.md"),
        "should contain first entry"
    );
    assert!(
        result.contains("feedback_tests.md"),
        "should contain second entry"
    );
    assert!(
        result.contains("project_sprint.md"),
        "should contain third entry"
    );
    assert!(
        result.contains("sprint 42 ends Friday"),
        "should contain entry descriptions"
    );
}

// ---------------------------------------------------------------------------
// TC-7.6: No MEMORY.md shows empty state
// ---------------------------------------------------------------------------

#[test]
fn tc_7_6_no_memory_md_shows_empty() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mem_dir = tmp.path().join("memory");
    fs::create_dir_all(&mem_dir).unwrap();
    // No MEMORY.md created

    let result = build_system_prompt(
        &mut SystemPromptCache::new(),
        None,
        "/tmp",
        "test-model",
        &[],
        None,
        Some(&mem_dir),
        false,
        false,
        false,
    );

    assert!(
        result.contains("currently empty"),
        "should show empty state when MEMORY.md doesn't exist"
    );
}

// ---------------------------------------------------------------------------
// TC-7.7: No bb brand identifiers in integrated prompt
// ---------------------------------------------------------------------------

#[test]
fn tc_7_7_no_bb_brand_in_integrated_prompt() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mem_dir = tmp.path().join("memory");
    fs::create_dir_all(&mem_dir).unwrap();
    fs::write(
        mem_dir.join("MEMORY.md"),
        "- [Test](test.md) \u{2014} entry\n",
    )
    .unwrap();

    let result = build_system_prompt(
        &mut SystemPromptCache::new(),
        None,
        "/tmp",
        "test-model",
        &[],
        None,
        Some(&mem_dir),
        false,
        false,
        false,
    );

    assert!(
        !result.contains("~/.claude"),
        "should not contain bb brand path ~/.claude"
    );
    assert!(
        !result.contains("CLAUDE.md"),
        "should not reference CLAUDE.md"
    );
}

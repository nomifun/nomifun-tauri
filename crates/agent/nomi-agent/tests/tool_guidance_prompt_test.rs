//! Integration tests for system prompt tool usage guidance (TC-4.3-01 through TC-4.3-08).
//!
//! Black-box tests verifying the "# Using your tools" section is correctly
//! assembled into the system prompt with proper content and ordering.

use std::fs;

use nomi_agent::context::{SystemPromptCache, build_system_prompt};
use nomi_skills::types::{ExecutionContext, LoadedFrom, SkillMetadata, SkillSource};

fn make_skill(name: &str, description: &str) -> SkillMetadata {
    SkillMetadata {
        name: name.to_string(),
        display_name: None,
        description: description.to_string(),
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
    }
}

// ---------------------------------------------------------------------------
// TC-4.3-01: Tool guidance section exists
// ---------------------------------------------------------------------------

#[test]
fn tc_4_3_01_tool_guidance_section_exists() {
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
        false, // browser_enabled
    );
    assert!(
        result.contains("# Using your tools"),
        "system prompt should contain the tool guidance section heading"
    );
}

// ---------------------------------------------------------------------------
// TC-4.3-02: Bash prohibition list with dedicated tool alternatives
// ---------------------------------------------------------------------------

#[test]
fn tc_4_3_02_bash_prohibition_list() {
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
        false, // browser_enabled
    );

    // Glob replaces find/ls
    assert!(
        result.contains("Glob") && result.contains("find"),
        "should map Glob as replacement for find"
    );
    // Grep replaces grep/rg
    assert!(
        result.contains("Grep") && result.contains("grep"),
        "should map Grep as replacement for grep"
    );
    // Read replaces cat/head/tail
    assert!(
        result.contains("Read") && result.contains("cat"),
        "should map Read as replacement for cat"
    );
    // Edit replaces sed/awk
    assert!(
        result.contains("Edit") && result.contains("sed"),
        "should map Edit as replacement for sed"
    );
    // Write replaces echo/heredoc
    assert!(
        result.contains("Write") && result.contains("echo"),
        "should map Write as replacement for echo"
    );
}

#[test]
fn tool_guidance_routes_directory_listing_to_glob() {
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
        false, // browser_enabled
    );
    let lower = result.to_lowercase();

    assert!(
        lower.contains("file listing") || lower.contains("list files"),
        "tool guidance should explicitly cover directory file listing"
    );
    assert!(
        lower.contains("every operating system") || lower.contains("os-agnostic"),
        "tool guidance should make directory listing OS-agnostic"
    );
    assert!(
        result.contains("Glob"),
        "tool guidance should route directory listing to Glob"
    );
    assert!(
        lower.contains("get-childitem") && lower.contains("ls") && lower.contains("dir"),
        "tool guidance should discourage shell listing commands"
    );
    assert!(
        result.contains("\"*\"") && result.contains("\"**/*\""),
        "tool guidance should teach top-level and recursive Glob listing patterns"
    );
}

// ---------------------------------------------------------------------------
// TC-4.3-03: Parallel call guidance
// ---------------------------------------------------------------------------

#[test]
fn tc_4_3_03_parallel_call_guidance() {
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
        false, // browser_enabled
    );
    assert!(
        result.contains("parallel"),
        "should contain parallel call guidance"
    );
    assert!(
        result.contains("sequentially"),
        "should explain when to run sequentially (dependencies)"
    );
}

#[test]
fn tc_4_3_03b_failure_checkpoint_guidance() {
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
        false, // browser_enabled
    );
    assert!(
        result.contains("hard checkpoint"),
        "tool guidance should make command errors a hard checkpoint"
    );
    assert!(
        result.contains("generous explicit timeout") && result.contains("exec_command/write_stdin"),
        "tool guidance should steer long-running commands toward larger timeouts or polling"
    );
}

#[test]
fn tool_call_efficiency_guidance_routes_batches_without_removing_checkpoints() {
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

    for required in [
        "file_paths",
        "ApplyPatch",
        "deterministic",
        "exec_command script mode",
        "intermediate result",
        "meaningful milestone",
    ] {
        assert!(result.contains(required), "missing efficiency rule: {required}");
    }
    assert!(result.contains("does not reduce the number of tool calls"));
    assert!(result.contains("Do not repeat"));
    assert!(result.contains("individual tool call"));
}

// ---------------------------------------------------------------------------
// TC-4.3-04: Edit-over-Write and Read-before-Edit rules
// ---------------------------------------------------------------------------

#[test]
fn tc_4_3_04_edit_write_read_rules() {
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
        false, // browser_enabled
    );
    assert!(
        result.contains("Prefer Edit over Write"),
        "should contain Edit-over-Write preference"
    );
    assert!(
        result.contains("Read a file before editing"),
        "should contain Read-before-Edit rule"
    );
}

// ---------------------------------------------------------------------------
// TC-4.3-05: Section order — guidance after intro, before custom prompt
// ---------------------------------------------------------------------------

#[test]
fn tc_4_3_05_order_after_intro_before_custom() {
    let result = build_system_prompt(
        &mut SystemPromptCache::new(),
        Some("CUSTOM_PROMPT_MARKER"),
        "/tmp",
        "test-model",
        &[],
        None,
        None,
        false,
        false,
        false, // browser_enabled
    );

    let intro_pos = result
        .find("You are an AI assistant")
        .expect("intro should contain the assistant identity line");
    let guidance_pos = result
        .find("# Using your tools")
        .expect("tool guidance section should exist");
    let custom_pos = result
        .find("CUSTOM_PROMPT_MARKER")
        .expect("custom prompt should exist");

    assert!(
        guidance_pos > intro_pos,
        "tool guidance should appear after the base intro"
    );
    assert!(
        guidance_pos < custom_pos,
        "tool guidance should appear before custom prompt"
    );
}

// ---------------------------------------------------------------------------
// TC-4.3-06: Section order — guidance before skills and memory
// ---------------------------------------------------------------------------

#[test]
fn tc_4_3_06_order_before_skills() {
    let skills = vec![make_skill("order-test-skill", "Order test")];
    let result = build_system_prompt(
        &mut SystemPromptCache::new(),
        None,
        "/tmp",
        "test-model",
        &skills,
        None,
        None,
        false,
        false,
        false, // browser_enabled
    );

    let guidance_pos = result
        .find("# Using your tools")
        .expect("tool guidance should exist");
    let skills_pos = result
        .find("order-test-skill")
        .expect("skill should be listed");

    assert!(
        guidance_pos < skills_pos,
        "tool guidance should appear before skills listing"
    );
}

#[test]
fn tc_4_3_06_order_before_memory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mem_dir = tmp.path().join("memory");
    fs::create_dir_all(&mem_dir).unwrap();
    fs::write(
        mem_dir.join("MEMORY.md"),
        "- [Note](note.md) \u{2014} some note\n",
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
        false, // browser_enabled
    );

    let guidance_pos = result
        .find("# Using your tools")
        .expect("tool guidance should exist");
    let memory_pos = result
        .find("auto memory")
        .expect("memory section should exist");

    assert!(
        guidance_pos < memory_pos,
        "tool guidance should appear before memory section"
    );
}

// ---------------------------------------------------------------------------
// TC-4.3-07: All sections coexist with correct ordering
// ---------------------------------------------------------------------------

#[test]
fn tc_4_3_07_all_sections_coexist() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cwd = tmp.path();

    // Create AGENTS.md
    fs::write(cwd.join("AGENTS.md"), "PROJECT_RULES_COEXIST").unwrap();

    // Create memory
    let mem_dir = tmp.path().join("memory");
    fs::create_dir_all(&mem_dir).unwrap();
    fs::write(
        mem_dir.join("MEMORY.md"),
        "- [Item](item.md) \u{2014} coexist test\n",
    )
    .unwrap();

    let skills = vec![make_skill("coexist-skill", "Coexist test")];
    let mut cache = SystemPromptCache::new();
    cache.set_agents_md(fs::read_to_string(cwd.join("AGENTS.md")).unwrap());

    let result = build_system_prompt(
        &mut cache,
        Some("CUSTOM_COEXIST"),
        &cwd.to_string_lossy(),
        "test-model",
        &skills,
        None,
        Some(&mem_dir),
        true, // plan mode active
        false,
        false, // browser_enabled
    );

    // All sections should exist
    assert!(result.contains("You are an AI assistant"), "intro should exist");
    assert!(
        result.contains("# Using your tools"),
        "tool guidance should exist"
    );
    assert!(
        result.contains("CUSTOM_COEXIST"),
        "custom prompt should exist"
    );
    assert!(
        result.contains("PROJECT_RULES_COEXIST"),
        "AGENTS.md should exist"
    );
    assert!(
        result.contains("auto memory"),
        "memory section should exist"
    );
    assert!(
        result.contains("coexist-skill"),
        "skills listing should exist"
    );

    // Verify ordering: intro < guidance < custom < agents.md < memory < skills
    let intro_pos = result.find("You are an AI assistant").unwrap();
    let guidance_pos = result.find("# Using your tools").unwrap();
    let custom_pos = result.find("CUSTOM_COEXIST").unwrap();
    let agents_pos = result.find("PROJECT_RULES_COEXIST").unwrap();
    let memory_pos = result.find("auto memory").unwrap();
    let skills_pos = result.find("coexist-skill").unwrap();

    assert!(guidance_pos > intro_pos, "guidance after intro");
    assert!(custom_pos > guidance_pos, "custom after guidance");
    assert!(agents_pos > custom_pos, "agents.md after custom");
    assert!(memory_pos > agents_pos, "memory after agents.md");
    assert!(skills_pos > memory_pos, "skills after memory");
}

// ---------------------------------------------------------------------------
// TC-4.3-08: Tool guidance present in plan mode
// ---------------------------------------------------------------------------

#[test]
fn tc_4_3_08_guidance_in_plan_mode() {
    let result = build_system_prompt(
        &mut SystemPromptCache::new(),
        None,
        "/tmp",
        "test-model",
        &[],
        None,
        None,
        true,
        false,
        false, // browser_enabled
    );
    assert!(
        result.contains("# Using your tools"),
        "tool guidance should be present even in plan mode"
    );
    // Plan mode instructions should also be present
    assert!(
        result.contains("plan") || result.contains("Plan"),
        "plan mode instructions should coexist with tool guidance"
    );
}

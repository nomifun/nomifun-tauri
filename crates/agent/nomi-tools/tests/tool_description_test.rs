//! Integration tests for enhanced tool descriptions (TC-4.2-01 through TC-4.2-08).
//!
//! These are black-box tests that verify each tool's description contains
//! the key guidance information specified in the test plan.

use std::path::PathBuf;

use nomi_tools::Tool;
use nomi_tools::bash::BashTool;
use nomi_tools::edit::EditTool;
use nomi_tools::glob::GlobTool;
use nomi_tools::grep::GrepTool;
use nomi_tools::read::ReadTool;
use nomi_tools::registry::ToolRegistry;
use nomi_tools::write::WriteTool;

fn test_cwd() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn bash_tool() -> BashTool {
    let cwd = test_cwd();
    BashTool::new(
        nomi_execution::ProcessSupervisor::new(
            nomi_execution::SupervisorConfig::default(),
        ),
        cwd.clone(),
        nomi_execution::CapabilityPolicy::local_owner(cwd),
    )
}

// --- TC-4.2-01: Bash tool description contains key guidance ---

#[test]
fn bash_description_references_dedicated_tools() {
    let tool = bash_tool();
    let desc = tool.description();
    assert!(
        desc.contains("Glob"),
        "Bash description should cross-reference Glob tool"
    );
    assert!(
        desc.contains("Grep"),
        "Bash description should cross-reference Grep tool"
    );
    assert!(
        desc.contains("Read"),
        "Bash description should cross-reference Read tool"
    );
    assert!(
        desc.contains("Edit"),
        "Bash description should cross-reference Edit tool"
    );
}

#[test]
fn bash_description_routes_file_listing_to_glob() {
    let tool = bash_tool();
    let desc = tool.description();
    let lower = desc.to_lowercase();
    assert!(
        lower.contains("file listing") || lower.contains("list files"),
        "Bash description should cover directory file listing, got: {desc}"
    );
    assert!(
        lower.contains("every operating system") || lower.contains("os-agnostic"),
        "Bash description should make file listing OS-agnostic, got: {desc}"
    );
    assert!(
        desc.contains("Glob"),
        "Bash description should route file listing to Glob"
    );
    assert!(
        lower.contains("get-childitem") && lower.contains("ls") && lower.contains("dir"),
        "Bash description should discourage shell listing commands, got: {desc}"
    );
}

#[test]
fn bash_description_contains_timeout_info() {
    let tool = bash_tool();
    let desc = tool.description();
    assert!(
        desc.contains("120") || desc.to_lowercase().contains("timeout"),
        "Bash description should mention timeout"
    );
}

#[test]
fn bash_description_contains_parallel_guidance() {
    let tool = bash_tool();
    let desc = tool.description();
    assert!(
        desc.contains("parallel") || desc.contains("&&"),
        "Bash description should contain parallel command guidance"
    );
}

#[test]
fn bash_description_avoids_fragile_inline_browser_test_harnesses() {
    let tool = bash_tool();
    let desc = tool.description();
    assert!(
        desc.contains("Browser") && desc.contains("node -e"),
        "Bash description should direct browser validation away from inline node -e mocks"
    );
    assert!(
        desc.contains("temporary script") && desc.contains("Write"),
        "Bash description should recommend a file-based script for multiline code"
    );
}

// --- TC-4.2-02: Read tool description contains usage constraints ---

#[test]
fn read_description_requires_absolute_path() {
    let tool = ReadTool::new(None, None);
    let desc = tool.description();
    assert!(
        desc.contains("absolute path"),
        "Read description should mention absolute path requirement"
    );
}

#[test]
fn read_description_mentions_line_numbers() {
    let tool = ReadTool::new(None, None);
    let desc = tool.description();
    assert!(
        desc.contains("line number"),
        "Read description should explain line number output format"
    );
}

#[test]
fn read_description_handles_binary() {
    let tool = ReadTool::new(None, None);
    let desc = tool.description();
    assert!(
        desc.to_lowercase().contains("binary"),
        "Read description should mention binary file handling"
    );
}

// --- TC-4.2-03: Edit tool description contains preconditions ---

#[test]
fn edit_description_requires_read_first() {
    let tool = EditTool::new(None);
    let desc = tool.description();
    assert!(
        desc.contains("Read"),
        "Edit description should require Read before editing"
    );
}

#[test]
fn edit_description_mentions_uniqueness() {
    let tool = EditTool::new(None);
    let desc = tool.description();
    assert!(
        desc.contains("unique"),
        "Edit description should mention old_string uniqueness requirement"
    );
}

#[test]
fn edit_description_mentions_replace_all() {
    let tool = EditTool::new(None);
    let desc = tool.description();
    assert!(
        desc.contains("replace_all"),
        "Edit description should document replace_all option"
    );
}

// --- TC-4.2-04: Write tool description contains operation semantics ---

#[test]
fn write_description_mentions_overwrite() {
    let tool = WriteTool::new(None);
    let desc = tool.description();
    assert!(
        desc.contains("overwrite") || desc.contains("overwrites"),
        "Write description should explain overwrite semantics"
    );
}

#[test]
fn write_description_requires_read_for_existing() {
    let tool = WriteTool::new(None);
    let desc = tool.description();
    assert!(
        desc.contains("Read"),
        "Write description should mention reading existing files first"
    );
}

#[test]
fn write_description_prefers_edit() {
    let tool = WriteTool::new(None);
    let desc = tool.description();
    assert!(
        desc.contains("Edit"),
        "Write description should recommend Edit for modifications"
    );
}

// --- TC-4.2-05: Glob tool description contains result limits ---

#[test]
fn glob_description_mentions_result_limit() {
    let tool = GlobTool::new(test_cwd());
    let desc = tool.description();
    assert!(
        desc.contains("100"),
        "Glob description should mention the 100 result limit"
    );
}

#[test]
fn glob_description_mentions_sort_order() {
    let tool = GlobTool::new(test_cwd());
    let desc = tool.description();
    let lower = desc.to_lowercase();
    assert!(
        lower.contains("modification time") || lower.contains("newest"),
        "Glob description should explain sort order"
    );
}

#[test]
fn glob_description_covers_directory_listing() {
    let tool = GlobTool::new(test_cwd());
    let desc = tool.description();
    let lower = desc.to_lowercase();
    assert!(
        lower.contains("list files"),
        "Glob description should explicitly support listing files, got: {desc}"
    );
    assert!(
        lower.contains("os-agnostic") || lower.contains("every operating system"),
        "Glob description should present directory listing as OS-agnostic, got: {desc}"
    );
    assert!(
        lower.contains("current directory") || lower.contains("current working directory"),
        "Glob description should mention current-directory listing, got: {desc}"
    );
    assert!(
        desc.contains("\"*\"") && desc.contains("\"**/*\""),
        "Glob description should teach top-level and recursive listing patterns, got: {desc}"
    );
}

// --- TC-4.2-06: Grep tool description contains mandatory usage rule ---

#[test]
fn grep_description_forbids_bash_grep() {
    let tool = GrepTool::new(test_cwd());
    let desc = tool.description();
    assert!(
        desc.contains("NEVER") || desc.contains("never"),
        "Grep description should forbid using grep in Bash"
    );
}

#[test]
fn grep_description_mentions_regex() {
    let tool = GrepTool::new(test_cwd());
    let desc = tool.description();
    assert!(
        desc.contains("regex"),
        "Grep description should mention regex support"
    );
}

#[test]
fn grep_description_mentions_result_limit() {
    let tool = GrepTool::new(test_cwd());
    let desc = tool.description();
    assert!(
        desc.contains("250"),
        "Grep description should mention the 250 result limit"
    );
}

// --- TC-4.3-09: Grep description accuracy fix (R-4.2-01) ---

#[test]
fn grep_description_does_not_say_at_most_matches() {
    let tool = GrepTool::new(test_cwd());
    let desc = tool.description();
    assert!(
        !desc.contains("at most 250 matches"),
        "Grep description should not say 'at most 250 matches' (was per-file, not global)"
    );
    assert!(
        desc.contains("capped at 250 lines"),
        "Grep description should accurately describe the 250-line cap"
    );
}

// --- TC-4.2-08: ToolDef propagation ---

#[test]
fn tool_def_description_matches_tool_instance() {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(bash_tool()));
    registry.register(Box::new(ReadTool::new(None, None)));
    registry.register(Box::new(EditTool::new(None)));
    registry.register(Box::new(WriteTool::new(None)));
    registry.register(Box::new(GlobTool::new(test_cwd())));
    registry.register(Box::new(GrepTool::new(test_cwd())));

    let defs = registry.to_tool_defs();

    for def in &defs {
        let tool = registry
            .get(&def.name)
            .expect("tool should exist in registry");
        assert_eq!(
            def.description,
            tool.description(),
            "ToolDef description for '{}' should match Tool::description()",
            def.name
        );
    }
}

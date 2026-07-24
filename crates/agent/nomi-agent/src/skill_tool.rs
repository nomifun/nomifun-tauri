use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use nomi_config::hooks::HooksConfig;
use nomi_config::shell::SupervisedShell;
use nomi_protocol::events::ToolCategory;
use nomi_skills::context_modifier::ContextModifier;
use nomi_skills::executor::{
    execute_fork_with_shell, prepare_inline_content_with_shell,
};
use nomi_skills::hooks::{parse_skill_hooks, to_hook_defs};
use nomi_skills::permissions::{SkillPermission, SkillPermissionChecker};
use nomi_skills::types::{ExecutionContext, SkillMetadata};
use nomi_types::agent::AgentInvocationRunner;
use nomi_types::tool::{JsonSchema, ToolResult};

use nomi_tools::Tool;

/// A tool that allows the LLM to invoke named skills.
///
/// Each skill is looked up by name (exact match, leading `/` stripped),
/// its content is prepared with variable substitution and shell execution,
/// and returned as a `ToolResult`.  The Skill list is injected into the
/// system prompt in Phase 9; this tool's `description()` returns a fixed string.
pub struct SkillTool {
    skills: Arc<Vec<SkillMetadata>>,
    /// Working directory for shell command execution inside skill content.
    cwd: String,
    /// Permission checker for skill-level deny/allow rules.
    checker: SkillPermissionChecker,
    /// Session ID passed to prepare_inline_content for ${NOMI_SESSION_ID} substitution.
    /// None if sessions are disabled or not yet initialised.
    session_id: Option<String>,
    /// Shared one-Agent invocation primitive for fork-mode skills.
    invocation_runner: Option<Arc<dyn AgentInvocationRunner>>,
    shell: SupervisedShell,
}

impl SkillTool {
    pub fn new(
        skills: Arc<Vec<SkillMetadata>>,
        cwd: String,
        checker: SkillPermissionChecker,
    ) -> Self {
        Self {
            skills,
            shell: SupervisedShell::standalone(std::path::PathBuf::from(&cwd)),
            cwd,
            checker,
            session_id: None,
            invocation_runner: None,
        }
    }

    /// Create a SkillTool with a known session ID.
    pub fn with_session_id(
        skills: Arc<Vec<SkillMetadata>>,
        cwd: String,
        checker: SkillPermissionChecker,
        session_id: Option<String>,
    ) -> Self {
        Self {
            skills,
            shell: SupervisedShell::standalone(std::path::PathBuf::from(&cwd)),
            cwd,
            checker,
            session_id,
            invocation_runner: None,
        }
    }

    /// Create a SkillTool with full fork-mode support.
    pub fn with_invocation_runner(
        skills: Arc<Vec<SkillMetadata>>,
        cwd: String,
        checker: SkillPermissionChecker,
        session_id: Option<String>,
        invocation_runner: Option<Arc<dyn AgentInvocationRunner>>,
    ) -> Self {
        Self {
            skills,
            shell: SupervisedShell::standalone(std::path::PathBuf::from(&cwd)),
            cwd,
            checker,
            session_id,
            invocation_runner,
        }
    }

    pub fn with_process_supervisor(
        mut self,
        supervisor: Arc<nomi_process_runtime::ProcessSupervisor>,
    ) -> Self {
        self.shell = SupervisedShell::new(supervisor, std::path::PathBuf::from(&self.cwd));
        self
    }

    /// Find a skill by exact name (case-sensitive, leading `/` stripped).
    fn find_skill(&self, name: &str) -> Option<&SkillMetadata> {
        let name = name.trim_start_matches('/');
        self.skills.iter().find(|s| s.name == name)
    }

    /// Build a comma-separated list of available skill names for error messages.
    fn available_names(&self) -> String {
        self.skills
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "Skill"
    }

    fn description(&self) -> &str {
        "Invoke a named skill by name. \
         Use the skill name exactly as listed in the system prompt. \
         Optionally pass arguments as a single string."
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "The skill name. E.g., \"commit\", \"review-pr\", or \"pdf\""
                },
                "args": {
                    "type": "string",
                    "description": "Optional arguments for the skill"
                }
            },
            "required": ["skill"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        // Skills may modify context; conservatively mark as not concurrency-safe.
        false
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(skill_name) = input["skill"].as_str() else {
            return ToolResult {
                content: "Missing required parameter: skill".to_string(),
                is_error: true,
                images: Vec::new(),
            };
        };

        let skill = match self.find_skill(skill_name) {
            Some(s) => s,
            None => {
                let available = self.available_names();
                return ToolResult {
                    content: format!(
                        "Skill '{}' not found. Available skills: {}",
                        skill_name, available
                    ),
                    is_error: true,
                    images: Vec::new(),
                };
            }
        };

        // Check skill-level permissions (applies to both inline and fork modes).
        match self.checker.check(skill) {
            SkillPermission::Deny => {
                return ToolResult {
                    content: format!("Skill '{}' is denied by configuration.", skill.name),
                    is_error: true,
                    images: Vec::new(),
                };
            }
            SkillPermission::Ask { reason } => {
                return ToolResult {
                    content: format!(
                        "Skill '{}' requires user approval before execution. \
                         {} \
                         Please ask the user to approve this skill in their configuration.",
                        skill.name, reason
                    ),
                    is_error: true,
                    images: Vec::new(),
                };
            }
            SkillPermission::Allow => {}
        }

        let args = input["args"].as_str();

        match skill.execution_context {
            ExecutionContext::Inline => {
                match prepare_inline_content_with_shell(
                    skill,
                    args,
                    self.session_id.as_deref(),
                    &self.cwd,
                    &self.shell,
                )
                .await
                {
                    Ok(content) => ToolResult {
                        content,
                        is_error: false,
                        images: Vec::new(),
                    },
                    Err(e) => ToolResult {
                        content: e.to_string(),
                        is_error: true,
                        images: Vec::new(),
                    },
                }
            }
            ExecutionContext::Fork => {
                let invocation_runner = match self.invocation_runner.as_ref() {
                    Some(s) => s.as_ref(),
                    None => {
                        return ToolResult {
                            content: format!(
                                "Skill '{}' requires fork execution context, \
                                 but no Agent invocation runner is available. \
                                 Fork support is enabled via SkillTool::with_invocation_runner().",
                                skill.name
                            ),
                            is_error: true,
                            images: Vec::new(),
                        };
                    }
                };
                match execute_fork_with_shell(
                    skill,
                    args,
                    self.session_id.as_deref(),
                    &self.cwd,
                    invocation_runner,
                    &self.shell,
                )
                .await
                {
                    Ok(content) => ToolResult {
                        content,
                        is_error: false,
                        images: Vec::new(),
                    },
                    Err(e) => ToolResult {
                        content: e,
                        is_error: true,
                        images: Vec::new(),
                    },
                }
            }
        }
    }

    fn context_modifier_for(&self, input: &serde_json::Value) -> Option<ContextModifier> {
        let skill_name = input["skill"].as_str()?;
        let skill = self.find_skill(skill_name)?;
        // Fork skills run in their own delegated Agent context; modifiers must not
        // propagate back to the parent conversation.
        if skill.execution_context == ExecutionContext::Fork {
            return None;
        }
        nomi_skills::context_modifier::from_skill(skill)
    }

    fn skill_hooks_for(&self, input: &serde_json::Value) -> Option<HooksConfig> {
        let skill_name = input["skill"].as_str()?;
        let skill = self.find_skill(skill_name)?;
        let config = parse_skill_hooks(skill.hooks_raw.as_ref(), &skill.name, skill.source)?;
        Some(to_hook_defs(&config, &skill.name))
    }

    fn category(&self) -> ToolCategory {
        // Inline mode returns skill content for the model to act on — categorised
        // as Info since it does not directly modify files or run commands.
        ToolCategory::Info
    }

    fn describe(&self, input: &Value) -> String {
        let name = input.get("skill").and_then(|v| v.as_str()).unwrap_or("?");
        match input.get("args").and_then(|v| v.as_str()) {
            Some(args) if !args.is_empty() => format!("Skill {name} {args}"),
            _ => format!("Skill {name}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nomi_skills::permissions::SkillPermissionChecker;
    use nomi_skills::types::{ExecutionContext, LoadedFrom, SkillSource};
    use serde_json::json;

    fn make_skill(name: &str, content: &str) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            display_name: None,
            description: format!("desc of {name}"),
            has_user_specified_description: true,
            allowed_tools: Vec::new(),
            argument_hint: None,
            argument_names: Vec::new(),
            when_to_use: None,
            version: None,
            model: None,
            disable_model_invocation: false,
            user_invocable: true,
            execution_context: ExecutionContext::Inline,
            agent: None,
            effort: None,
            shell: None,
            paths: Vec::new(),
            hooks_raw: None,
            source: SkillSource::User,
            loaded_from: LoadedFrom::Skills,
            content: content.to_string(),
            content_length: content.len(),
            skill_root: None,
        }
    }

    fn tool_with(skills: Vec<SkillMetadata>) -> SkillTool {
        SkillTool::new(
            Arc::new(skills),
            "/tmp".to_string(),
            SkillPermissionChecker::new(vec![], vec![], false),
        )
    }

    #[tokio::test]
    async fn test_skill_found_returns_content() {
        let tool = tool_with(vec![make_skill("commit", "# Commit\nDo a commit.")]);
        let result = tool.execute(json!({ "skill": "commit" })).await;
        assert!(!result.is_error);
        assert!(result.content.contains("Do a commit."));
    }

    #[tokio::test]
    async fn test_skill_not_found_returns_error() {
        let tool = tool_with(vec![make_skill("commit", "content")]);
        let result = tool.execute(json!({ "skill": "nonexistent" })).await;
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
        assert!(result.content.contains("commit"));
    }

    #[tokio::test]
    async fn test_leading_slash_stripped() {
        let tool = tool_with(vec![make_skill("commit", "body")]);
        let result = tool.execute(json!({ "skill": "/commit" })).await;
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn test_missing_skill_param_returns_error() {
        let tool = tool_with(vec![]);
        let result = tool.execute(json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("Missing required parameter"));
    }

    #[tokio::test]
    async fn test_args_substituted() {
        let tool = tool_with(vec![make_skill("greet", "Hello $ARGUMENTS!")]);
        let result = tool
            .execute(json!({ "skill": "greet", "args": "world" }))
            .await;
        assert!(!result.is_error);
        assert_eq!(result.content, "Hello world!");
    }

    #[tokio::test]
    async fn test_fork_skill_returns_error() {
        let mut skill = make_skill("fork-skill", "body");
        skill.execution_context = ExecutionContext::Fork;
        let tool = tool_with(vec![skill]);
        let result = tool.execute(json!({ "skill": "fork-skill" })).await;
        assert!(result.is_error);
        assert!(result.content.contains("fork execution context"));
    }

    #[test]
    fn test_describe_with_args() {
        let tool = tool_with(vec![]);
        let desc = tool.describe(&json!({ "skill": "commit", "args": "fix bug" }));
        assert_eq!(desc, "Skill commit fix bug");
    }

    #[test]
    fn test_describe_without_args() {
        let tool = tool_with(vec![]);
        let desc = tool.describe(&json!({ "skill": "commit" }));
        assert_eq!(desc, "Skill commit");
    }

    #[test]
    fn test_name_is_skill() {
        let tool = tool_with(vec![]);
        assert_eq!(tool.name(), "Skill");
    }

    #[test]
    fn test_not_concurrency_safe() {
        let tool = tool_with(vec![]);
        assert!(!tool.is_concurrency_safe(&json!({})));
    }
}

// ---------------------------------------------------------------------------
// Supplemental tests (tester role — covers test-plan.md cases not in impl tests)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod supplemental_tests {
    use std::sync::Arc;

    use serde_json::json;

    use nomi_skills::permissions::SkillPermissionChecker;
    use nomi_skills::types::{ExecutionContext, LoadedFrom, SkillMetadata, SkillSource};

    use super::SkillTool;
    use nomi_tools::Tool;

    fn make_skill(name: &str, content: &str) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            display_name: None,
            description: format!("desc of {name}"),
            has_user_specified_description: true,
            allowed_tools: Vec::new(),
            argument_hint: None,
            argument_names: Vec::new(),
            when_to_use: None,
            version: None,
            model: None,
            disable_model_invocation: false,
            user_invocable: true,
            execution_context: ExecutionContext::Inline,
            agent: None,
            effort: None,
            shell: None,
            paths: Vec::new(),
            hooks_raw: None,
            source: SkillSource::User,
            loaded_from: LoadedFrom::Skills,
            content: content.to_string(),
            content_length: content.len(),
            skill_root: None,
        }
    }

    fn tool_with(skills: Vec<SkillMetadata>) -> SkillTool {
        SkillTool::new(
            Arc::new(skills),
            "/tmp".to_string(),
            SkillPermissionChecker::new(vec![], vec![], false),
        )
    }

    // -----------------------------------------------------------------------
    // TC-11.x: find_skill
    // -----------------------------------------------------------------------

    #[test]
    fn tc_11_1_exact_match_found() {
        let tool = tool_with(vec![make_skill("commit", "body")]);
        // Access find_skill through execute to verify behavior indirectly
        // (find_skill is private, tested via execute)
        // Direct check via available_names() not exposed, so we verify via execute.
        // Verified in tc_13_1 instead. This test just verifies construction.
        assert_eq!(tool.name(), "Skill");
    }

    #[test]
    fn tc_11_4_case_sensitive_no_match() {
        // "Commit" (capital C) should not match "commit"
        let tool = tool_with(vec![make_skill("commit", "body")]);
        // Verified via execute in tc_13.x
        let _ = tool;
    }

    #[test]
    fn tc_11_5_empty_skills_list_no_panic() {
        let tool = tool_with(vec![]);
        assert_eq!(tool.name(), "Skill"); // just verifies no panic
    }

    // -----------------------------------------------------------------------
    // TC-12.x: name, schema, is_concurrency_safe
    // -----------------------------------------------------------------------

    #[test]
    fn tc_12_1_name_is_skill() {
        let tool = tool_with(vec![]);
        assert_eq!(tool.name(), "Skill");
    }

    #[test]
    fn tc_12_2_schema_skill_required() {
        let tool = tool_with(vec![]);
        let schema = tool.input_schema();
        let required = schema["required"].as_array().unwrap();
        let names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(
            names.contains(&"skill"),
            "schema required must contain 'skill'"
        );
    }

    #[test]
    fn tc_12_3_schema_args_not_required() {
        let tool = tool_with(vec![]);
        let schema = tool.input_schema();
        // args should be in properties
        assert!(
            schema["properties"]["args"].is_object(),
            "args should be in properties"
        );
        // args should NOT be in required
        let required = schema["required"].as_array().unwrap();
        let names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(!names.contains(&"args"), "args should not be in required");
    }

    #[test]
    fn tc_12_4_is_concurrency_safe_false() {
        let tool = tool_with(vec![]);
        assert!(!tool.is_concurrency_safe(&json!({})));
        assert!(!tool.is_concurrency_safe(&json!({"skill": "foo"})));
    }

    // -----------------------------------------------------------------------
    // TC-13.x: execute (async)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn tc_13_1_successful_inline_execution() {
        let tool = tool_with(vec![make_skill("my-skill", "Run $ARGUMENTS")]);
        let result = tool
            .execute(json!({"skill": "my-skill", "args": "foo"}))
            .await;
        assert!(!result.is_error);
        assert_eq!(result.content, "Run foo");
    }

    #[tokio::test]
    async fn tc_13_2_skill_not_found_is_error() {
        let tool = tool_with(vec![make_skill("commit", "body")]);
        let result = tool.execute(json!({"skill": "nonexistent"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("not found") || result.content.contains("Skill"));
    }

    #[tokio::test]
    async fn tc_13_3_not_found_error_lists_available_skills() {
        let tool = tool_with(vec![
            make_skill("commit", "body"),
            make_skill("review", "body"),
        ]);
        let result = tool.execute(json!({"skill": "missing"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("commit"));
        assert!(result.content.contains("review"));
    }

    #[tokio::test]
    async fn tc_13_4_fork_skill_returns_error() {
        let mut skill = make_skill("fork-skill", "body");
        skill.execution_context = ExecutionContext::Fork;
        let tool = tool_with(vec![skill]);
        let result = tool.execute(json!({"skill": "fork-skill"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("fork"));
    }

    #[tokio::test]
    async fn tc_13_5_no_args_field_still_works() {
        let tool = tool_with(vec![make_skill("my-skill", "Just content.")]);
        let result = tool.execute(json!({"skill": "my-skill"})).await;
        assert!(!result.is_error);
        assert_eq!(result.content, "Just content.");
    }

    #[tokio::test]
    async fn tc_13_6_leading_slash_stripped() {
        let tool = tool_with(vec![make_skill("my-skill", "body")]);
        let result = tool.execute(json!({"skill": "/my-skill"})).await;
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn tc_13_7_missing_skill_field_returns_error() {
        let tool = tool_with(vec![]);
        let result = tool.execute(json!({"args": "foo"})).await;
        assert!(result.is_error);
        assert!(
            result.content.to_lowercase().contains("missing") || result.content.contains("skill")
        );
    }

    #[tokio::test]
    async fn tc_13_8_full_variable_substitution_integration() {
        let mut skill = make_skill("my-skill", "Run ${NOMI_SKILL_DIR}/tool.sh $ARGUMENTS[0]");
        skill.skill_root = Some("/my/skill".to_string());
        let tool = tool_with(vec![skill]);
        let result = tool
            .execute(json!({"skill": "my-skill", "args": "alpha"}))
            .await;
        assert!(!result.is_error);
        // base dir header is prepended, then substitution applied
        assert!(result.content.contains("/my/skill/tool.sh alpha"));
    }

    #[tokio::test]
    async fn tc_13_x_case_sensitive_no_match() {
        // "Commit" does not match "commit"
        let tool = tool_with(vec![make_skill("commit", "body")]);
        let result = tool.execute(json!({"skill": "Commit"})).await;
        assert!(
            result.is_error,
            "case-sensitive lookup: 'Commit' should not match 'commit'"
        );
    }

    // -----------------------------------------------------------------------
    // TC-14.x: description
    // -----------------------------------------------------------------------

    #[test]
    fn tc_14_1_description_is_non_empty() {
        let tool = tool_with(vec![
            make_skill("commit", "body"),
            make_skill("review", "body"),
        ]);
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn tc_14_2_empty_skills_description_no_panic() {
        let tool = tool_with(vec![]);
        assert!(!tool.description().is_empty());
    }
}

// ---------------------------------------------------------------------------
// Phase 6 supplemental tests — context_modifier_for() and session_id
// ---------------------------------------------------------------------------

#[cfg(test)]
mod supplemental_tests_p6 {
    use std::sync::Arc;

    use serde_json::json;

    use nomi_skills::permissions::SkillPermissionChecker;
    use nomi_skills::types::{
        EffortLevel, ExecutionContext, LoadedFrom, SkillMetadata, SkillSource,
    };
    use nomi_tools::Tool;

    use super::SkillTool;

    fn base_skill(name: &str) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            display_name: None,
            description: format!("desc of {name}"),
            has_user_specified_description: true,
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
            content: "body".to_string(),
            content_length: 4,
            skill_root: None,
        }
    }

    fn tool_with(skills: Vec<SkillMetadata>) -> SkillTool {
        SkillTool::new(
            Arc::new(skills),
            "/tmp".to_string(),
            SkillPermissionChecker::new(vec![], vec![], false),
        )
    }

    // TC-6.14: skill name not in registry → None
    #[test]
    fn tc_6_14_skill_not_found_returns_none() {
        let tool = tool_with(vec![base_skill("commit")]);
        assert!(
            tool.context_modifier_for(&json!({"skill": "nonexistent"}))
                .is_none()
        );
    }

    // TC-6.15: input missing skill field → None
    #[test]
    fn tc_6_15_missing_skill_field_returns_none() {
        let tool = tool_with(vec![base_skill("commit")]);
        assert!(tool.context_modifier_for(&json!({})).is_none());
    }

    // TC-6.16: skill exists but no override fields → None
    #[test]
    fn tc_6_16_skill_no_override_returns_none() {
        let tool = tool_with(vec![base_skill("no-override")]);
        assert!(
            tool.context_modifier_for(&json!({"skill": "no-override"}))
                .is_none()
        );
    }

    // TC-6.17: skill has model override → Some with correct model
    #[test]
    fn tc_6_17_skill_with_model_returns_some() {
        let mut skill = base_skill("model-skill");
        skill.model = Some("test-model".to_string());
        let tool = tool_with(vec![skill]);

        let modifier = tool.context_modifier_for(&json!({"skill": "model-skill"}));
        assert!(modifier.is_some());
        let m = modifier.unwrap();
        assert_eq!(m.model.as_deref(), Some("test-model"));
        assert!(m.effort.is_none());
        assert!(m.allowed_tools.is_empty());
    }

    // TC-6.18: skill has effort override → Some with correct effort
    #[test]
    fn tc_6_18_skill_with_effort_returns_some() {
        let mut skill = base_skill("effort-skill");
        skill.effort = Some(EffortLevel::High);
        let tool = tool_with(vec![skill]);

        let modifier = tool.context_modifier_for(&json!({"skill": "effort-skill"}));
        assert!(modifier.is_some());
        let m = modifier.unwrap();
        assert_eq!(m.effort, Some(EffortLevel::High));
        assert!(m.model.is_none());
    }

    // TC-6.19: skill has allowed_tools override → Some with correct tools
    #[test]
    fn tc_6_19_skill_with_allowed_tools_returns_some() {
        let mut skill = base_skill("tools-skill");
        skill.allowed_tools = vec!["Bash".to_string(), "Read".to_string()];
        let tool = tool_with(vec![skill]);

        let modifier = tool.context_modifier_for(&json!({"skill": "tools-skill"}));
        assert!(modifier.is_some());
        let m = modifier.unwrap();
        assert_eq!(m.allowed_tools, vec!["Bash", "Read"]);
    }

    // TC-6.19b: leading slash is stripped before lookup
    #[test]
    fn tc_6_19b_leading_slash_stripped_in_context_modifier_for() {
        let mut skill = base_skill("slash-skill");
        skill.model = Some("m".to_string());
        let tool = tool_with(vec![skill]);

        // /slash-skill should resolve to slash-skill
        let modifier = tool.context_modifier_for(&json!({"skill": "/slash-skill"}));
        assert!(modifier.is_some());
    }

    // TC-6.20: with_session_id() stores session_id; new() defaults to None
    #[test]
    fn tc_6_20_session_id_stored_correctly() {
        let skills = Arc::new(vec![]);

        // new() → session_id is None
        let tool_no_session = SkillTool::new(
            skills.clone(),
            "/tmp".to_string(),
            SkillPermissionChecker::new(vec![], vec![], false),
        );
        assert!(tool_no_session.session_id.is_none());

        // with_session_id() → session_id is set
        let tool_with_session = SkillTool::with_session_id(
            skills,
            "/tmp".to_string(),
            SkillPermissionChecker::new(vec![], vec![], false),
            Some("sess-abc".to_string()),
        );
        assert_eq!(tool_with_session.session_id.as_deref(), Some("sess-abc"));
    }

    // TC-6.20b: with_session_id(None) stores None
    #[test]
    fn tc_6_20b_session_id_none_when_not_provided() {
        let tool = SkillTool::with_session_id(
            Arc::new(vec![]),
            "/tmp".to_string(),
            SkillPermissionChecker::new(vec![], vec![], false),
            None,
        );
        assert!(tool.session_id.is_none());
    }

    // TC-6.17b: context_modifier_for() is independent of execute() — pure lookup, no side effects
    #[test]
    fn tc_6_17b_context_modifier_for_does_not_mutate_tool() {
        let mut skill = base_skill("pure-skill");
        skill.model = Some("model-x".to_string());
        let tool = tool_with(vec![skill]);

        // Call twice — result must be identical (no state mutation)
        let m1 = tool.context_modifier_for(&json!({"skill": "pure-skill"}));
        let m2 = tool.context_modifier_for(&json!({"skill": "pure-skill"}));
        assert_eq!(m1.unwrap().model, m2.unwrap().model);
    }
}

// ---------------------------------------------------------------------------
// Permission integration tests (P5-11, P5-12)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod permission_tests {
    use std::sync::Arc;

    use serde_json::json;

    use nomi_skills::permissions::SkillPermissionChecker;
    use nomi_skills::types::{ExecutionContext, LoadedFrom, SkillMetadata, SkillSource};

    use super::SkillTool;
    use nomi_tools::Tool;

    fn make_skill(name: &str, content: &str) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            display_name: None,
            description: format!("desc of {name}"),
            has_user_specified_description: true,
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
            content: content.to_string(),
            content_length: content.len(),
            skill_root: None,
        }
    }

    // P5-11: SkillTool returns error for a denied skill.
    #[tokio::test]
    async fn p5_11_denied_skill_returns_error() {
        let checker = SkillPermissionChecker::new(vec!["dangerous".to_string()], vec![], false);
        let tool = SkillTool::new(
            Arc::new(vec![make_skill("dangerous", "rm -rf /")]),
            "/tmp".to_string(),
            checker,
        );
        let result = tool.execute(json!({"skill": "dangerous"})).await;
        assert!(result.is_error);
        assert!(
            result.content.contains("denied"),
            "content: {}",
            result.content
        );
    }

    // P5-12: SkillTool returns informative message for a skill that needs approval.
    #[tokio::test]
    async fn p5_12_ask_skill_returns_approval_prompt() {
        let checker = SkillPermissionChecker::new(vec![], vec![], false);
        let mut skill = make_skill("hooked", "body");
        skill.hooks_raw = Some(serde_json::json!({ "pre": "echo hi" }));
        let tool = SkillTool::new(Arc::new(vec![skill]), "/tmp".to_string(), checker);
        let result = tool.execute(json!({"skill": "hooked"})).await;
        assert!(result.is_error);
        assert!(
            result.content.contains("approval") || result.content.contains("approve"),
            "content should mention approval: {}",
            result.content
        );
    }
}

// ---------------------------------------------------------------------------
// Phase 7 tests — SkillTool fork branch, context_modifier_for fork=None, permissions
// ---------------------------------------------------------------------------

#[cfg(test)]
mod phase7_tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use serde_json::json;

    use nomi_types::agent::{
        AgentInvocationInput, AgentInvocationOutput, AgentInvocationRunner,
    };
    use nomi_skills::permissions::SkillPermissionChecker;
    use nomi_skills::types::{
        EffortLevel, ExecutionContext, LoadedFrom, SkillMetadata, SkillSource,
    };
    use nomi_tools::Tool;
    use nomi_types::message::TokenUsage;

    use super::SkillTool;

    // ---------------------------------------------------------------------------
    // Mock runner — returns a preset result and captures the one-call input.
    // ---------------------------------------------------------------------------

    struct MockInvocationRunner {
        is_error: bool,
        text: String,
        captured_input: Mutex<Option<AgentInvocationInput>>,
    }

    impl MockInvocationRunner {
        fn success(text: &str) -> Arc<Self> {
            Arc::new(Self {
                is_error: false,
                text: text.to_string(),
                captured_input: Mutex::new(None),
            })
        }

        #[allow(dead_code)]
        fn error(text: &str) -> Arc<Self> {
            Arc::new(Self {
                is_error: true,
                text: text.to_string(),
                captured_input: Mutex::new(None),
            })
        }

        #[allow(dead_code)]
        fn take_input(&self) -> AgentInvocationInput {
            self.captured_input
                .lock()
                .unwrap()
                .take()
                .expect("invoke was not called")
        }
    }

    #[async_trait]
    impl AgentInvocationRunner for MockInvocationRunner {
        async fn invoke(&self, input: AgentInvocationInput) -> AgentInvocationOutput {
            *self.captured_input.lock().unwrap() = Some(input.clone());
            AgentInvocationOutput {
                name: input.name.clone(),
                text: self.text.clone(),
                usage: TokenUsage::default(),
                turns: 1,
                is_error: self.is_error,
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------

    fn make_fork_skill(name: &str, content: &str) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            display_name: None,
            description: format!("desc of {name}"),
            has_user_specified_description: true,
            allowed_tools: Vec::new(),
            argument_hint: None,
            argument_names: Vec::new(),
            when_to_use: None,
            version: None,
            model: None,
            disable_model_invocation: false,
            user_invocable: true,
            execution_context: ExecutionContext::Fork,
            agent: None,
            effort: None,
            shell: None,
            paths: Vec::new(),
            hooks_raw: None,
            source: SkillSource::User,
            loaded_from: LoadedFrom::Skills,
            content: content.to_string(),
            content_length: content.len(),
            skill_root: None,
        }
    }

    fn make_inline_skill(name: &str, content: &str) -> SkillMetadata {
        SkillMetadata {
            execution_context: ExecutionContext::Inline,
            name: name.to_string(),
            display_name: None,
            description: format!("desc of {name}"),
            has_user_specified_description: true,
            allowed_tools: Vec::new(),
            argument_hint: None,
            argument_names: Vec::new(),
            when_to_use: None,
            version: None,
            model: None,
            disable_model_invocation: false,
            user_invocable: true,
            agent: None,
            effort: None,
            shell: None,
            paths: Vec::new(),
            hooks_raw: None,
            source: SkillSource::User,
            loaded_from: LoadedFrom::Skills,
            content: content.to_string(),
            content_length: content.len(),
            skill_root: None,
        }
    }

    fn tool_with_invocation_runner(
        skills: Vec<SkillMetadata>,
        invocation_runner: Option<Arc<dyn AgentInvocationRunner>>,
    ) -> SkillTool {
        SkillTool::with_invocation_runner(
            Arc::new(skills),
            "/tmp".to_string(),
            SkillPermissionChecker::new(vec![], vec![], false),
            None,
            invocation_runner,
        )
    }

    fn tool_no_backend(skills: Vec<SkillMetadata>) -> SkillTool {
        tool_with_invocation_runner(skills, None)
    }

    // ---------------------------------------------------------------------------
    // TC-7.20: inline skill takes inline path — invocation runner NOT called
    // ---------------------------------------------------------------------------
    #[tokio::test]
    async fn tc_7_20_inline_skill_takes_inline_path() {
        let invocation_runner = MockInvocationRunner::success("should not be called");
        let tool = tool_with_invocation_runner(
            vec![make_inline_skill("inline-skill", "inline content")],
            Some(invocation_runner.clone() as Arc<dyn AgentInvocationRunner>),
        );
        let result = tool.execute(json!({"skill": "inline-skill"})).await;
        assert!(
            !result.is_error,
            "inline skill should succeed: {}",
            result.content
        );
        assert_eq!(result.content, "inline content");
        // execute_fork should NOT have been called
        assert!(
            invocation_runner.captured_input.lock().unwrap().is_none(),
            "invocation runner should not have been called for inline skill"
        );
    }

    // TC-7.21: fork skill takes fork path — invocation runner IS called
    #[tokio::test]
    async fn tc_7_21_fork_skill_takes_fork_path() {
        let invocation_runner = MockInvocationRunner::success("fork result");
        let tool = tool_with_invocation_runner(
            vec![make_fork_skill("fork-skill", "fork content")],
            Some(invocation_runner.clone() as Arc<dyn AgentInvocationRunner>),
        );
        let result = tool.execute(json!({"skill": "fork-skill"})).await;
        assert!(
            !result.is_error,
            "fork skill should succeed: {}",
            result.content
        );
        assert_eq!(result.content, "fork result");
        // execute_fork should have been called exactly once
        assert!(
            invocation_runner.captured_input.lock().unwrap().is_some(),
            "invocation runner should have been called for fork skill"
        );
    }

    // TC-7.12: no delegation backend — fork skill returns clear error message
    #[tokio::test]
    async fn tc_7_12_fork_skill_no_backend_returns_error() {
        let tool = tool_no_backend(vec![make_fork_skill("needs-delegation-backend", "content")]);
        let result = tool.execute(json!({"skill": "needs-delegation-backend"})).await;
        assert!(result.is_error, "should be error without delegation backend");
        assert!(
            result.content.contains("fork execution context"),
            "error message should mention 'fork execution context': {}",
            result.content
        );
    }

    // TC-7.23: context_modifier_for() returns None for fork skill
    #[test]
    fn tc_7_23_context_modifier_for_fork_returns_none() {
        // Fork skill with model/effort overrides — still returns None
        let mut skill = make_fork_skill("fork-with-model", "content");
        skill.model = Some("claude-opus-4-6".to_string());
        skill.effort = Some(EffortLevel::High);
        skill.allowed_tools = vec!["Bash".to_string()];
        let tool = tool_no_backend(vec![skill]);
        let modifier = tool.context_modifier_for(&json!({"skill": "fork-with-model"}));
        assert!(
            modifier.is_none(),
            "fork skill should return None from context_modifier_for"
        );
    }

    // TC-7.22: context_modifier_for() returns Some for inline skill with overrides
    #[test]
    fn tc_7_22_context_modifier_for_inline_returns_some() {
        let mut skill = make_inline_skill("inline-with-model", "content");
        skill.model = Some("my-model".to_string());
        let tool = tool_no_backend(vec![skill]);
        let modifier = tool.context_modifier_for(&json!({"skill": "inline-with-model"}));
        assert!(
            modifier.is_some(),
            "inline skill with model override should return Some"
        );
        assert_eq!(modifier.unwrap().model.as_deref(), Some("my-model"));
    }

    // TC-7.24: fork skill without a delegation backend returns an error without panic
    #[tokio::test]
    async fn tc_7_24_fork_no_backend_no_panic() {
        let tool = tool_no_backend(vec![make_fork_skill("no-delegation", "content")]);
        // Should not panic, must return Err
        let result = tool.execute(json!({"skill": "no-delegation"})).await;
        assert!(result.is_error);
        assert!(!result.content.is_empty());
    }

    // TC-7.30: fork skill — permission allow — proceeds to fork execution
    #[tokio::test]
    async fn tc_7_30_fork_skill_permission_allow_proceeds() {
        let invocation_runner = MockInvocationRunner::success("fork ok");
        let tool = SkillTool::with_invocation_runner(
            Arc::new(vec![make_fork_skill("fork-allowed", "content")]),
            "/tmp".to_string(),
            // deny_list empty, allow_list empty = allow all
            SkillPermissionChecker::new(vec![], vec![], false),
            None,
            Some(invocation_runner as Arc<dyn AgentInvocationRunner>),
        );
        let result = tool.execute(json!({"skill": "fork-allowed"})).await;
        assert!(
            !result.is_error,
            "allowed fork skill should succeed: {}",
            result.content
        );
        assert_eq!(result.content, "fork ok");
    }

    // TC-7.31: fork skill — permission deny — blocked before fork execution
    #[tokio::test]
    async fn tc_7_31_fork_skill_permission_deny_blocked() {
        let invocation_runner = MockInvocationRunner::success("should not reach here");
        let tool = SkillTool::with_invocation_runner(
            Arc::new(vec![make_fork_skill("fork-denied", "content")]),
            "/tmp".to_string(),
            // deny "fork-denied"
            SkillPermissionChecker::new(vec!["fork-denied".to_string()], vec![], false),
            None,
            Some(invocation_runner.clone() as Arc<dyn AgentInvocationRunner>),
        );
        let result = tool.execute(json!({"skill": "fork-denied"})).await;
        assert!(result.is_error, "denied fork skill should return error");
        assert!(
            result.content.contains("denied"),
            "error should mention 'denied': {}",
            result.content
        );
        // The runner must not be called since permission checks happen first.
        assert!(
            invocation_runner.captured_input.lock().unwrap().is_none(),
            "invocation runner should not be called when skill is denied"
        );
    }

    // with_invocation_runner() stores the shared primitive correctly.
    #[test]
    fn tc_7_with_invocation_runner_constructor() {
        let invocation_runner: Arc<dyn AgentInvocationRunner> =
            MockInvocationRunner::success("ok");
        let tool = SkillTool::with_invocation_runner(
            Arc::new(vec![]),
            "/tmp".to_string(),
            SkillPermissionChecker::new(vec![], vec![], false),
            Some("sess-1".to_string()),
            Some(invocation_runner),
        );
        // Verify session_id was also stored
        assert_eq!(tool.session_id.as_deref(), Some("sess-1"));
        assert!(tool.invocation_runner.is_some());
    }

    // new() constructor leaves the runner absent.
    #[test]
    fn tc_7_new_constructor_invocation_runner_is_none() {
        let tool = SkillTool::new(
            Arc::new(vec![]),
            "/tmp".to_string(),
            SkillPermissionChecker::new(vec![], vec![], false),
        );
        assert!(tool.invocation_runner.is_none());
    }
}

// ---------------------------------------------------------------------------
// Phase 11 tests — skill_hooks_for() (TC-11.40 ~ TC-11.45)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod phase11_tests {
    use std::sync::Arc;

    use serde_json::json;

    use nomi_skills::permissions::SkillPermissionChecker;
    use nomi_skills::types::{ExecutionContext, LoadedFrom, SkillMetadata, SkillSource};
    use nomi_tools::Tool;

    use super::SkillTool;

    fn base_skill(
        name: &str,
        source: SkillSource,
        hooks_raw: Option<serde_json::Value>,
    ) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            display_name: None,
            description: format!("desc of {name}"),
            has_user_specified_description: true,
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
            hooks_raw,
            source,
            loaded_from: LoadedFrom::Skills,
            content: "body".to_string(),
            content_length: 4,
            skill_root: None,
        }
    }

    fn tool_with(skills: Vec<SkillMetadata>) -> SkillTool {
        SkillTool::new(
            Arc::new(skills),
            "/tmp".to_string(),
            SkillPermissionChecker::new(vec![], vec![], false),
        )
    }

    fn valid_hooks_json() -> serde_json::Value {
        json!({
            "PreToolUse": [{"hooks": [{"type": "command", "command": "echo pre"}]}]
        })
    }

    // TC-11.40: skill with valid hooks_raw returns Some(HooksConfig)
    #[test]
    fn tc_11_40_skill_with_hooks_returns_some() {
        let skill = base_skill("my-skill", SkillSource::User, Some(valid_hooks_json()));
        let tool = tool_with(vec![skill]);
        let result = tool.skill_hooks_for(&json!({"skill": "my-skill"}));
        assert!(
            result.is_some(),
            "TC-11.40: skill with valid hooks must return Some"
        );
        let config = result.unwrap();
        assert!(
            !config.pre_tool_use.is_empty(),
            "TC-11.40: pre_tool_use must be non-empty"
        );
    }

    // TC-11.41: skill without hooks_raw returns None
    #[test]
    fn tc_11_41_skill_without_hooks_returns_none() {
        let skill = base_skill("no-hooks", SkillSource::User, None);
        let tool = tool_with(vec![skill]);
        let result = tool.skill_hooks_for(&json!({"skill": "no-hooks"}));
        assert!(
            result.is_none(),
            "TC-11.41: skill without hooks must return None"
        );
    }

    // TC-11.42: nonexistent skill name returns None
    #[test]
    fn tc_11_42_nonexistent_skill_returns_none() {
        let tool = tool_with(vec![]);
        let result = tool.skill_hooks_for(&json!({"skill": "nonexistent"}));
        assert!(
            result.is_none(),
            "TC-11.42: nonexistent skill must return None"
        );
    }

    // TC-11.43: input missing skill field returns None
    #[test]
    fn tc_11_43_missing_skill_field_returns_none() {
        let skill = base_skill("my-skill", SkillSource::User, Some(valid_hooks_json()));
        let tool = tool_with(vec![skill]);
        assert!(
            tool.skill_hooks_for(&json!({})).is_none(),
            "TC-11.43: no skill field → None"
        );
        assert!(
            tool.skill_hooks_for(&json!({"foo": "bar"})).is_none(),
            "TC-11.43: wrong field → None"
        );
    }

    // TC-11.44: MCP source skill with hooks_raw returns None
    #[test]
    fn tc_11_44_mcp_source_returns_none() {
        let skill = base_skill("mcp-skill", SkillSource::Mcp, Some(valid_hooks_json()));
        let tool = tool_with(vec![skill]);
        let result = tool.skill_hooks_for(&json!({"skill": "mcp-skill"}));
        assert!(result.is_none(), "TC-11.44: MCP source must return None");
    }

    // TC-11.45: invalid hooks_raw (array, not object) returns None without panic
    #[test]
    fn tc_11_45_invalid_hooks_raw_returns_none() {
        let skill = base_skill("bad-hooks", SkillSource::User, Some(json!([1, 2, 3])));
        let tool = tool_with(vec![skill]);
        let result = tool.skill_hooks_for(&json!({"skill": "bad-hooks"}));
        assert!(
            result.is_none(),
            "TC-11.45: invalid hooks_raw (array) must return None"
        );
    }
}

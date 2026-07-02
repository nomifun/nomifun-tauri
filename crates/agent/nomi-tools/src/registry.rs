use nomi_types::tool::ToolDef;

use crate::Tool;

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Find a tool by name
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    /// Get all registered tool names
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    /// Generate API tool definitions for all registered tools
    pub fn to_tool_defs(&self) -> Vec<ToolDef> {
        self.tools
            .iter()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
                deferred: t.is_deferred(),
            })
            .collect()
    }

    /// Generate API tool definitions for tools matching a predicate.
    ///
    /// Used by plan mode to restrict the tool set sent to the LLM.
    pub fn to_tool_defs_filtered<F>(&self, filter: F) -> Vec<ToolDef>
    where
        F: Fn(&dyn Tool) -> bool,
    {
        self.tools
            .iter()
            .filter(|t| filter(t.as_ref()))
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
                deferred: t.is_deferred(),
            })
            .collect()
    }

    /// Keep only the tools whose name is in `allowed`. An EMPTY `allowed` is a
    /// no-op (= unrestricted), so callers can pass a config value straight
    /// through. Used for restricted sub-agent worker sessions (per-node tool
    /// whitelist) — registration-time filtering, because the registry has no
    /// unregister.
    pub fn retain_named(&mut self, allowed: &[String]) {
        if allowed.is_empty() {
            return;
        }
        self.tools.retain(|t| allowed.iter().any(|a| a == t.name()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;
    use async_trait::async_trait;
    use nomi_protocol::events::ToolCategory;
    use nomi_types::tool::ToolResult;

    /// A minimal Tool implementation used only in tests
    struct MockTool {
        tool_name: String,
        tool_description: String,
        tool_category: ToolCategory,
    }

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            &self.tool_name
        }

        fn description(&self) -> &str {
            &self.tool_description
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            true
        }

        async fn execute(&self, _input: serde_json::Value) -> ToolResult {
            ToolResult::text("ok")
        }

        fn category(&self) -> ToolCategory {
            self.tool_category
        }
    }

    /// Helper to create a MockTool with the given name and description
    fn make_tool(name: &str, description: &str) -> Box<MockTool> {
        Box::new(MockTool {
            tool_name: name.to_string(),
            tool_description: description.to_string(),
            tool_category: ToolCategory::Info,
        })
    }

    fn make_tool_with_category(
        name: &str,
        description: &str,
        category: ToolCategory,
    ) -> Box<MockTool> {
        Box::new(MockTool {
            tool_name: name.to_string(),
            tool_description: description.to_string(),
            tool_category: category,
        })
    }

    #[test]
    fn test_register_and_get() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("my_tool", "does something"));

        let found = registry.get("my_tool");
        assert!(
            found.is_some(),
            "registered tool should be retrievable by name"
        );
        assert_eq!(found.unwrap().name(), "my_tool");
    }

    #[test]
    fn test_get_nonexistent_returns_none() {
        let registry = ToolRegistry::new();

        let result = registry.get("ghost");
        assert!(
            result.is_none(),
            "looking up an unregistered name should return None"
        );
    }

    #[test]
    fn test_tool_names() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("alpha", "first tool"));
        registry.register(make_tool("beta", "second tool"));
        registry.register(make_tool("gamma", "third tool"));

        let mut names = registry.tool_names();
        names.sort(); // sort for a stable assertion order
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn test_to_tool_defs() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("tool_a", "description A"));
        registry.register(make_tool("tool_b", "description B"));

        let defs = registry.to_tool_defs();
        assert_eq!(
            defs.len(),
            2,
            "to_tool_defs should return one entry per registered tool"
        );

        // Collect (name, description) pairs for assertion independent of order
        let mut pairs: Vec<(&str, &str)> = defs
            .iter()
            .map(|d| (d.name.as_str(), d.description.as_str()))
            .collect();
        pairs.sort();

        assert_eq!(pairs[0], ("tool_a", "description A"));
        assert_eq!(pairs[1], ("tool_b", "description B"));

        // Verify the input_schema field is populated correctly
        let expected_schema = serde_json::json!({"type": "object"});
        for def in &defs {
            assert_eq!(def.input_schema, expected_schema);
        }
    }

    // --- to_tool_defs_filtered tests ---

    #[test]
    fn filtered_by_category_returns_matching_tools() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool_with_category(
            "Read",
            "read files",
            ToolCategory::Info,
        ));
        registry.register(make_tool_with_category(
            "Write",
            "write files",
            ToolCategory::Edit,
        ));
        registry.register(make_tool_with_category(
            "Bash",
            "run commands",
            ToolCategory::Exec,
        ));
        registry.register(make_tool_with_category(
            "ExitPlanMode",
            "exit plan mode",
            ToolCategory::Info,
        ));

        let defs = registry.to_tool_defs_filtered(|t| t.category() == ToolCategory::Info);

        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"Read"));
        assert!(names.contains(&"ExitPlanMode"));
        assert!(!names.contains(&"Write"));
        assert!(!names.contains(&"Bash"));
    }

    #[test]
    fn filtered_by_name_excludes_specific_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("alpha", "first"));
        registry.register(make_tool("beta", "second"));
        registry.register(make_tool("gamma", "third"));

        let defs = registry.to_tool_defs_filtered(|t| t.name() != "beta");

        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"gamma"));
        assert!(!names.contains(&"beta"));
    }

    #[test]
    fn filtered_accept_all_matches_to_tool_defs() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("a", "tool a"));
        registry.register(make_tool("b", "tool b"));

        let all = registry.to_tool_defs();
        let filtered = registry.to_tool_defs_filtered(|_| true);

        assert_eq!(all.len(), filtered.len());
        for (a, f) in all.iter().zip(filtered.iter()) {
            assert_eq!(a.name, f.name);
        }
    }

    #[test]
    fn filtered_reject_all_returns_empty() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("a", "tool a"));

        let defs = registry.to_tool_defs_filtered(|_| false);
        assert!(defs.is_empty());
    }

    #[test]
    fn filtered_empty_registry_returns_empty() {
        let registry = ToolRegistry::new();
        let defs = registry.to_tool_defs_filtered(|_| true);
        assert!(defs.is_empty());
    }

    // --- retain_named (per-node tool whitelist) tests ---

    #[test]
    fn retain_named_keeps_only_allowed_and_empty_is_noop() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("Glob", "find files"));
        registry.register(make_tool("Grep", "search content"));
        registry.register(make_tool("Bash", "run commands"));

        // 空 allowlist = 不限制（默认，零回归）。
        registry.retain_named(&[]);
        assert!(registry.get("Glob").is_some());
        assert!(registry.get("Grep").is_some());
        assert!(registry.get("Bash").is_some());

        // 非空 = 只保留白名单内的（含 MCP 代理等一切已注册工具）。
        registry.retain_named(&["Glob".to_string(), "Grep".to_string()]);
        assert!(registry.get("Glob").is_some());
        assert!(registry.get("Grep").is_some());
        assert!(registry.get("Bash").is_none(), "白名单外的工具必须被移除");
        assert_eq!(registry.tool_names().len(), 2);
    }

    // --- deferred flag tests ---

    /// A minimal Tool that overrides is_deferred() to return true
    struct DeferredMockTool {
        tool_name: String,
    }

    #[async_trait]
    impl Tool for DeferredMockTool {
        fn name(&self) -> &str {
            &self.tool_name
        }

        fn description(&self) -> &str {
            "a deferred tool"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"x": {"type": "string"}}})
        }

        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            true
        }

        fn is_deferred(&self) -> bool {
            true
        }

        async fn execute(&self, _input: serde_json::Value) -> ToolResult {
            ToolResult::text("ok")
        }

        fn category(&self) -> ToolCategory {
            ToolCategory::Info
        }
    }

    #[test]
    fn to_tool_defs_includes_deferred_flag() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool("core_tool", "a core tool"));
        let defs = registry.to_tool_defs();
        assert!(!defs[0].deferred, "default tools should not be deferred");
    }

    #[test]
    fn to_tool_defs_deferred_tool_flagged() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DeferredMockTool {
            tool_name: "lazy_tool".to_string(),
        }));
        let defs = registry.to_tool_defs();
        assert!(defs[0].deferred, "deferred tool should have deferred=true");
    }
}

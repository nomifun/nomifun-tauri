pub mod apply_patch;
pub mod bash;
pub mod edit;
pub mod exec_command;
pub mod file_cache;
pub mod glob;
pub mod grep;
pub mod lsp;
pub mod output_truncation;
pub mod path_guard;
#[cfg(test)]
pub mod persistent_shell;
pub mod process_store;
#[cfg(test)]
pub mod pty;
pub mod read;
pub mod registry;
pub mod tool_search;
pub mod update_plan;
pub mod worktree;
pub mod write;
pub mod write_stdin;
pub(crate) mod windows_shell;

#[cfg(test)]
mod windows_shell_tests {
    use nomi_process_runtime::Transport;

    use super::windows_shell::{shell_transport, validate_shell_script};

    #[test]
    fn windows_shell_uses_pty_even_when_tty_is_not_requested() {
        let transport = shell_transport(false);
        #[cfg(windows)]
        assert_eq!(transport, Transport::Pty { cols: 120, rows: 30 });
        #[cfg(not(windows))]
        assert_eq!(transport, Transport::Pipe);
    }

    #[test]
    fn windows_launch_policy_leaves_quoted_data_and_cmd_c_alone() {
        assert!(validate_shell_script("cmd /c echo ok").is_ok());
        assert!(validate_shell_script("Write-Output 'cmd /k is data'").is_ok());
        #[cfg(windows)]
        {
            assert!(validate_shell_script("start cmd").is_err());
            assert!(validate_shell_script("Start-Process notepad").is_err());
            assert!(validate_shell_script("cmd /k echo hi").is_err());
            assert!(validate_shell_script("cmd /c start notepad").is_err());
        }
    }
}

/// Shared test-only helpers (path to the cross-platform `pty_test_helper` bin).
#[cfg(test)]
pub(crate) mod test_support;

pub use output_truncation::{TruncationBudget, approx_token_count, truncate_middle};

use async_trait::async_trait;
use serde_json::Value;

use nomi_config::hooks::HooksConfig;
use nomi_protocol::events::ToolCategory;
use nomi_types::skill_types::ContextModifier;
use nomi_types::tool::{JsonSchema, ToolResult};

/// Truncate a string to at most `max_bytes`, snapping to a char boundary.
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Write `content` to `file_path` atomically: write to a uniquely-named temp
/// file in the same directory, then rename it over the target. Rename is atomic
/// on the same filesystem, so a crash or a concurrent reader never observes a
/// half-written file. Falls back to a direct write only if the rename fails
/// (e.g. cross-device). Shared by the Edit and Write tools so both get the same
/// crash-safety guarantee.
pub(crate) fn atomic_write(file_path: &str, content: &str) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = format!("{}.tmp.{}.{}", file_path, std::process::id(), seq);

    if let Err(e) = std::fs::write(&tmp_path, content) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    if std::fs::rename(&tmp_path, file_path).is_err() {
        // Cross-device rename (temp and target on different filesystems) cannot
        // be atomic; clean up the temp and fall back to a direct write.
        let _ = std::fs::remove_file(&tmp_path);
        std::fs::write(file_path, content)?;
    }
    Ok(())
}

/// Trusted identity of one provider-emitted tool invocation.
///
/// This value is constructed by the engine from `ContentBlock::ToolUse.id`,
/// outside the model-visible tool input object. Tools therefore cannot accept
/// or override it through their JSON schema. The raw provider identifier is
/// hashed into a bounded, log-safe token before it crosses any MCP or HTTP
/// boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolExecutionContext {
    operation_id: String,
}

impl ToolExecutionContext {
    const DOMAIN: &'static [u8] = b"nomifun-tool-execution:v1\0";

    pub fn from_tool_call_id(tool_call_id: &str) -> Self {
        Self::from_scoped_tool_call("", tool_call_id)
    }

    /// Derive an invocation identity from both the durable turn/message scope
    /// and the provider's call id. Some providers reuse short ids such as
    /// `call_0` in later turns, so the scope is part of the hash.
    pub fn from_scoped_tool_call(execution_scope: &str, tool_call_id: &str) -> Self {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(Self::DOMAIN);
        hasher.update((execution_scope.len() as u64).to_be_bytes());
        hasher.update(execution_scope.as_bytes());
        hasher.update((tool_call_id.len() as u64).to_be_bytes());
        hasher.update(tool_call_id.as_bytes());
        let digest = hasher.finalize();
        Self {
            operation_id: format!("tool-call-v1-{digest:x}"),
        }
    }

    /// Stable, bounded visible-ASCII identity for this exact tool invocation.
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
}

/// A tool that the agent can invoke
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (must match API schema)
    fn name(&self) -> &str;

    /// Stable identity used to persist deferred-schema activation across
    /// sessions. Native tools default to their provider-visible name; tools
    /// whose provider-visible name is derived from another origin (for example
    /// MCP aliases) must override this with that immutable origin identity.
    fn activation_identity(&self) -> &str {
        self.name()
    }

    /// Untruncated semantic identity used to derive output-delivery contracts.
    /// Native tools default to their provider-visible name. Tool families whose
    /// provider route is a bounded/hash alias (notably MCP) must override this
    /// with the complete origin identity so a suffix such as `export_pdf`
    /// cannot disappear at the provider name-length boundary.
    fn artifact_identity(&self) -> &str {
        self.name()
    }

    /// Reserved provider-name prefix owned by this tool family. Registries use
    /// this to keep origin-stable namespaces from being claimed by unrelated
    /// native tools before a dynamic tool registers.
    fn reserved_provider_name_prefix(&self) -> Option<&'static str> {
        None
    }

    /// Additional informational terms used only by deferred ToolSearch. These
    /// aliases never participate in registration policy, dispatch, or approval
    /// decisions; those always use the unique provider-visible [`Self::name`].
    fn deferred_search_aliases(&self) -> Vec<String> {
        Vec::new()
    }

    /// Human-readable description for the LLM
    fn description(&self) -> &str;

    /// JSON Schema for input parameters
    fn input_schema(&self) -> JsonSchema;

    /// Whether this tool is safe to run concurrently
    fn is_concurrency_safe(&self, input: &Value) -> bool;

    /// Execute the tool
    async fn execute(&self, input: Value) -> ToolResult;

    /// Execute with engine-owned invocation identity.
    ///
    /// Native tools keep their existing implementation through this default.
    /// Boundary tools such as MCP proxies override it to propagate durable
    /// idempotency without adding model-controlled input fields.
    async fn execute_with_context(
        &self,
        input: Value,
        _context: &ToolExecutionContext,
    ) -> ToolResult {
        self.execute(input).await
    }

    /// Return an optional context modifier based on the tool input.
    /// Called after execute() to collect any engine-level overrides.
    /// Only SkillTool overrides this; all other tools return None.
    fn context_modifier_for(&self, _input: &Value) -> Option<ContextModifier> {
        None
    }

    /// Return any hooks declared in the skill's frontmatter for dynamic registration.
    /// Called after a successful execute() so the tool-execution layer can merge
    /// the returned hooks into the active HookEngine.
    /// Only SkillTool overrides this; all other tools return None.
    fn skill_hooks_for(&self, _input: &Value) -> Option<HooksConfig> {
        None
    }

    /// Max result size in chars before truncation
    fn max_result_size(&self) -> usize {
        50_000
    }

    /// Tool category for protocol classification
    fn category(&self) -> ToolCategory;

    /// Category for a specific invocation. Lets multi-action tools (e.g.
    /// Computer/Browser) report read-only actions as Info so approval
    /// gating can distinguish them from mutating actions.
    fn category_for(&self, _input: &Value) -> ToolCategory {
        self.category()
    }

    /// Whether this specific invocation can skip interactive approval even when
    /// the session is not globally auto-approved. Defaults to false so existing
    /// tools keep their current approval behavior.
    fn auto_approve_invocation(&self, _input: &Value, _category: ToolCategory) -> bool {
        false
    }

    /// Whether an unchanged successful result can be a normal part of waiting
    /// for external progress. Polling invocations are excluded from the
    /// stagnation guard, but remain bounded by the engine's turn safety net.
    fn is_polling_invocation(&self, _input: &Value) -> bool {
        false
    }

    /// Whether this tool's schema should be deferred (sent as name-only stub).
    /// Override to `true` for tools with large schemas or infrequent use.
    fn is_deferred(&self) -> bool {
        false
    }

    /// Human-readable description of what the tool will do with the given input
    fn describe(&self, input: &Value) -> String {
        format!(
            "{}: {}",
            self.name(),
            serde_json::to_string(input).unwrap_or_default()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_execution_context_is_stable_bounded_and_turn_scoped() {
        let first = ToolExecutionContext::from_scoped_tool_call("turn-a", "call_0");
        let retry = ToolExecutionContext::from_scoped_tool_call("turn-a", "call_0");
        let next_turn = ToolExecutionContext::from_scoped_tool_call("turn-b", "call_0");
        let next_call = ToolExecutionContext::from_scoped_tool_call("turn-a", "call_1");

        assert_eq!(first, retry);
        assert_ne!(first, next_turn);
        assert_ne!(first, next_call);
        assert!(first.operation_id().len() <= 128);
        assert!(
            first
                .operation_id()
                .bytes()
                .all(|byte| (0x21..=0x7e).contains(&byte))
        );
    }

    #[test]
    fn truncate_utf8_ascii_within_limit() {
        assert_eq!(truncate_utf8("hello", 80), "hello");
    }

    #[test]
    fn truncate_utf8_ascii_at_boundary() {
        assert_eq!(truncate_utf8("abcde", 3), "abc");
    }

    #[test]
    fn truncate_utf8_multibyte_snaps_back() {
        // '些' is 3 bytes (E4 BA 9B) starting at index 79 would span 79..82
        let s = "# 用 script 模拟 TTY 交互来添加 DeepSeek 提供商\n# 首先看看有哪些";
        let result = truncate_utf8(s, 80);
        assert!(result.len() <= 80);
        assert!(result.is_char_boundary(result.len()));
    }

    #[test]
    fn truncate_utf8_empty() {
        assert_eq!(truncate_utf8("", 80), "");
    }

    #[test]
    fn truncate_utf8_zero_limit() {
        assert_eq!(truncate_utf8("hello", 0), "");
    }

    #[test]
    fn truncate_utf8_emoji() {
        // 🦀 is 4 bytes
        let s = "aaa🦀bbb";
        assert_eq!(truncate_utf8(s, 4), "aaa");
        assert_eq!(truncate_utf8(s, 7), "aaa🦀");
    }

}

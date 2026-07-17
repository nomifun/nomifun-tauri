use std::fmt;
use std::str::FromStr;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::message::TokenUsage;

/// The one lifecycle vocabulary for an Agent Execution, independent of
/// whether a platform host persists it or an embedded host projects it
/// synchronously for the current turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentExecutionStatus {
    Planning,
    AwaitingApproval,
    Running,
    Paused,
    WaitingInput,
    Completed,
    CompletedWithFailures,
    Failed,
    Cancelled,
}

impl AgentExecutionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Planning => "planning",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::WaitingInput => "waiting_input",
            Self::Completed => "completed",
            Self::CompletedWithFailures => "completed_with_failures",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::CompletedWithFailures | Self::Failed | Self::Cancelled
        )
    }

    /// The complete legal transition table for a persisted execution. A no-op
    /// update is accepted so idempotent recovery commands remain safe.
    pub const fn can_transition_to(self, next: Self) -> bool {
        if self as u8 == next as u8 {
            return true;
        }
        match self {
            Self::Planning => matches!(
                next,
                Self::AwaitingApproval | Self::Running | Self::Failed | Self::Cancelled
            ),
            Self::AwaitingApproval => {
                matches!(next, Self::Running | Self::Failed | Self::Cancelled)
            }
            Self::Running => matches!(
                next,
                Self::AwaitingApproval
                    | Self::Paused
                    | Self::WaitingInput
                    | Self::Completed
                    | Self::CompletedWithFailures
                    | Self::Failed
                    | Self::Cancelled
            ),
            Self::Paused => matches!(
                next,
                Self::AwaitingApproval
                    | Self::Running
                    | Self::WaitingInput
                    | Self::Failed
                    | Self::Cancelled
            ),
            Self::WaitingInput => matches!(
                next,
                Self::AwaitingApproval
                    | Self::Running
                    | Self::Paused
                    | Self::Failed
                    | Self::Cancelled
            ),
            // A settled result may be explicitly reopened by a versioned
            // retry/adopt command. Runtime attempt settlement still refuses
            // to touch a settled aggregate, so late events cannot revive it.
            Self::Completed | Self::CompletedWithFailures | Self::Failed => {
                matches!(next, Self::Running)
            }
            Self::Cancelled => false,
        }
    }
}

impl fmt::Display for AgentExecutionStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Parse failure for the canonical Agent Execution lifecycle vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownAgentExecutionStatus(String);

impl fmt::Display for UnknownAgentExecutionStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown AgentExecutionStatus value: {}", self.0)
    }
}

impl std::error::Error for UnknownAgentExecutionStatus {}

impl FromStr for AgentExecutionStatus {
    type Err = UnknownAgentExecutionStatus;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "planning" => Ok(Self::Planning),
            "awaiting_approval" => Ok(Self::AwaitingApproval),
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            "waiting_input" => Ok(Self::WaitingInput),
            "completed" => Ok(Self::Completed),
            "completed_with_failures" => Ok(Self::CompletedWithFailures),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(UnknownAgentExecutionStatus(value.to_owned())),
        }
    }
}

/// Minimal receipt returned by every successful `nomi_delegate` deployment.
///
/// Platform deployments may return an active status while durable work runs;
/// embedded deployments return a terminal projection after synchronous work.
/// Deployment names are deliberately absent from the wire contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentExecutionReceipt {
    pub execution_id: String,
    pub status: AgentExecutionStatus,
    pub message: String,
    /// Steps appended to an already-running execution. Empty for a new
    /// execution and for embedded terminal projection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub step_ids: Vec<String>,
    /// Terminal per-step outputs returned by an embedded deployment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<AgentExecutionStepResult>,
    /// Optional read-only consolidation output from an embedded deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesis: Option<AgentExecutionStepResult>,
    /// Terminal aggregate counts and usage from an embedded deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<AgentExecutionSummary>,
}

impl AgentExecutionReceipt {
    pub fn new(
        execution_id: impl Into<String>,
        status: AgentExecutionStatus,
        message: impl Into<String>,
    ) -> Self {
        Self {
            execution_id: execution_id.into(),
            status,
            message: message.into(),
            step_ids: Vec::new(),
            results: Vec::new(),
            synthesis: None,
            summary: None,
        }
    }

    pub fn with_step_ids(mut self, step_ids: Vec<String>) -> Self {
        self.step_ids = step_ids;
        self
    }

    pub fn with_terminal_projection(
        mut self,
        summary: AgentExecutionSummary,
        results: Vec<AgentExecutionStepResult>,
        synthesis: Option<AgentExecutionStepResult>,
    ) -> Self {
        self.summary = Some(summary);
        self.results = results;
        self.synthesis = synthesis;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentExecutionStepResultStatus {
    Completed,
    Failed,
}

/// Terminal projection of one embedded execution step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentExecutionStepResult {
    pub name: String,
    pub status: AgentExecutionStepResultStatus,
    pub text: String,
    pub turns: usize,
    pub usage: TokenUsage,
}

impl From<&AgentInvocationOutput> for AgentExecutionStepResult {
    fn from(output: &AgentInvocationOutput) -> Self {
        Self {
            name: output.name.clone(),
            status: if output.is_error {
                AgentExecutionStepResultStatus::Failed
            } else {
                AgentExecutionStepResultStatus::Completed
            },
            text: output.text.clone(),
            turns: output.turns,
            usage: output.usage.clone(),
        }
    }
}

/// Aggregate terminal projection for an embedded Agent Execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentExecutionSummary {
    pub step_count: usize,
    pub completed_count: usize,
    pub failed_count: usize,
    pub usage: TokenUsage,
}

/// Fixed wire discriminator for the shared parallel delegation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ParallelDelegationStrategy {
    Parallel,
}

/// The one `strategy=parallel` request DTO consumed by every
/// `nomi_delegate` deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ParallelDelegationRequest {
    /// Exactly `parallel`. Parallel requests never include planned aggregate
    /// settings such as `work_dir`, `model_pool`, or `max_parallel`.
    pub strategy: ParallelDelegationStrategy,
    /// A native JSON array of independent Agent task objects, never a JSON string.
    #[schemars(length(min = 1, max = 16))]
    pub tasks: Vec<AgentDelegationTask>,
    /// Whether to add one synthesis step after every task completes.
    #[serde(default)]
    pub synthesize: bool,
}

impl ParallelDelegationRequest {
    pub const MAX_TASKS: usize = 16;

    /// Enforce the semantic constraints represented in JSON Schema for callers
    /// that deserialize outside a schema-aware model runtime.
    pub fn validate(&self) -> Result<(), String> {
        if self.tasks.is_empty() || self.tasks.len() > Self::MAX_TASKS {
            return Err(format!(
                "parallel delegation requires 1-{} tasks",
                Self::MAX_TASKS
            ));
        }
        for (index, task) in self.tasks.iter().enumerate() {
            task.validate()
                .map_err(|error| format!("parallel task {index}: {error}"))?;
        }
        Ok(())
    }
}

/// Explicit per-Agent narrowing of inherited tool authority.
///
/// This type is the single policy vocabulary shared by embedded invocation and
/// persisted Agent Execution. A free-form role is descriptive metadata only
/// and must never be interpreted as permission.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentToolPolicy {
    #[default]
    Full,
    ReadOnly,
    ReadShell,
}

impl AgentToolPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::ReadOnly => "read_only",
            Self::ReadShell => "read_shell",
        }
    }
}

impl std::fmt::Display for AgentToolPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for AgentToolPolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "full" => Ok(Self::Full),
            "read_only" => Ok(Self::ReadOnly),
            "read_shell" => Ok(Self::ReadShell),
            _ => Err(format!("unknown Agent tool policy '{value}'")),
        }
    }
}

/// One task in the shared `strategy=parallel` request vocabulary.
///
/// Both embedded and platform hosts deserialize this exact DTO. `role` is
/// intentionally free-form; `tool_policy` is the only field that narrows
/// tools. When present, `role` is injected as explicit invocation context by
/// both deployments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentDelegationTask {
    /// Short descriptive name shown in output or the execution canvas.
    pub name: String,
    /// Complete task prompt for the invoked Agent.
    pub prompt: String,
    /// Optional human-readable role used as invocation context and for
    /// persistent routing/display. It never grants tool authority.
    #[serde(default)]
    pub role: Option<String>,
    /// Explicit narrowing of inherited tool authority.
    #[serde(default)]
    pub tool_policy: AgentToolPolicy,
}

/// Apply the one shared execution meaning of a delegation role.
///
/// Role is prompt context, not a permission or a second Agent type. Taking an
/// owned prompt lets hosts use this without cloning, and keeping the exact
/// wording here prevents embedded and persisted execution from drifting.
pub fn apply_agent_role_context(prompt: String, role: Option<&str>) -> String {
    let Some(role) = role.map(str::trim).filter(|role| !role.is_empty()) else {
        return prompt;
    };
    format!(
        "DELEGATED ROLE CONTEXT: {role}\n\
         This role describes the focus for this invocation and does not grant or widen tool permissions.\n\n\
         {prompt}"
    )
}

impl AgentDelegationTask {
    /// Validate task fields identically for embedded and platform deployments.
    /// Keeping this next to the shared wire DTO prevents one host from
    /// accepting a task another rejects.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("Agent delegation task name must not be blank".to_owned());
        }
        if self.prompt.trim().is_empty() {
            return Err("Agent delegation task prompt must not be blank".to_owned());
        }
        if self.role.as_ref().is_some_and(|role| role.trim().is_empty()) {
            return Err("Agent delegation task role must not be blank when provided".to_owned());
        }
        Ok(())
    }
}

/// Input for one Agent invocation primitive.
///
/// This is intentionally below both embedded fan-out and skill fork
/// mode. It describes one call, not an execution aggregate or a second Agent
/// product. `exact_tools` exists for skill metadata and is intersected with
/// both the inherited host scope and `tool_policy`.
#[derive(Debug, Clone)]
pub struct AgentInvocationInput {
    pub name: String,
    pub prompt: String,
    pub max_turns: usize,
    pub max_tokens: u32,
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub tool_policy: AgentToolPolicy,
    /// Optional exact tool set from a skill manifest. Empty means no additional
    /// restriction; a non-empty set is always intersected and can never widen.
    pub exact_tools: Vec<String>,
}

/// Output from one Agent invocation primitive.
#[derive(Debug)]
pub struct AgentInvocationOutput {
    pub name: String,
    pub text: String,
    pub usage: TokenUsage,
    pub turns: usize,
    pub is_error: bool,
}

/// Executes one Agent invocation.
///
/// Fan-out strategy, persistence, private progress projection, and worktree
/// isolation live in hosts above this seam. Skill fork mode uses the same
/// primitive.
#[async_trait]
pub trait AgentInvocationRunner: Send + Sync {
    async fn invoke(&self, input: AgentInvocationInput) -> AgentInvocationOutput;
}

#[cfg(test)]
mod tests {
    use super::{
        AgentDelegationTask, AgentExecutionReceipt, AgentExecutionStatus,
        AgentExecutionStepResult, AgentExecutionStepResultStatus, AgentExecutionSummary,
        AgentToolPolicy, ParallelDelegationRequest, apply_agent_role_context,
    };
    use crate::message::TokenUsage;

    #[test]
    fn shared_task_role_is_free_form_and_policy_is_strict() {
        let task: AgentDelegationTask = serde_json::from_value(serde_json::json!({
            "name": "实现",
            "prompt": "修改代码",
            "role": "中文自定义角色",
            "tool_policy": "full"
        }))
        .unwrap();
        assert_eq!(task.role.as_deref(), Some("中文自定义角色"));
        assert_eq!(task.tool_policy, AgentToolPolicy::Full);

        assert!(serde_json::from_value::<AgentDelegationTask>(serde_json::json!({
            "name": "bad",
            "prompt": "bad",
            "tool_policy": "admin"
        }))
        .is_err());
    }

    #[test]
    fn shared_task_blank_validation_is_canonical() {
        let task = |name: &str, prompt: &str, role: Option<&str>| AgentDelegationTask {
            name: name.to_owned(),
            prompt: prompt.to_owned(),
            role: role.map(str::to_owned),
            tool_policy: AgentToolPolicy::Full,
        };
        assert!(task("name", "prompt", Some("任意角色")).validate().is_ok());
        assert!(task(" ", "prompt", None).validate().is_err());
        assert!(task("name", "\n", None).validate().is_err());
        assert!(task("name", "prompt", Some("\t")).validate().is_err());
    }

    #[test]
    fn role_context_has_one_non_authorizing_prompt_semantic() {
        let prompt = apply_agent_role_context(
            "Inspect the persistence boundary.".to_owned(),
            Some("  database reviewer  "),
        );
        assert!(prompt.starts_with("DELEGATED ROLE CONTEXT: database reviewer\n"));
        assert!(prompt.contains("does not grant or widen tool permissions"));
        assert!(prompt.ends_with("Inspect the persistence boundary."));
        assert_eq!(
            apply_agent_role_context("unchanged".to_owned(), None),
            "unchanged"
        );
    }

    #[test]
    fn parallel_request_is_one_strict_shared_wire_contract() {
        let request: ParallelDelegationRequest = serde_json::from_value(serde_json::json!({
            "strategy": "parallel",
            "tasks": [{"name":"scan","prompt":"inspect"}],
            "synthesize": true
        }))
        .unwrap();
        assert!(request.validate().is_ok());
        assert!(serde_json::from_value::<ParallelDelegationRequest>(serde_json::json!({
            "strategy": "parallel",
            "tasks": [{"name":"scan","prompt":"inspect"}],
            "work_dir": "/model/must/not/choose"
        }))
        .is_err());
    }

    #[test]
    fn receipt_omits_deployment_specific_empty_projection_fields() {
        let receipt = AgentExecutionReceipt::new(
            "exec_test",
            AgentExecutionStatus::Running,
            "accepted",
        );
        let value = serde_json::to_value(receipt).unwrap();
        assert_eq!(value["execution_id"], "exec_test");
        assert_eq!(value["status"], "running");
        assert_eq!(value["message"], "accepted");
        for absent in ["mode", "execution_mode", "step_ids", "results", "synthesis", "summary"] {
            assert!(value.get(absent).is_none(), "unexpected field {absent}: {value}");
        }
    }

    #[test]
    fn receipt_terminal_projection_and_step_ids_are_typed_extensions() {
        let result = AgentExecutionStepResult {
            name: "scan".to_owned(),
            status: AgentExecutionStepResultStatus::Completed,
            text: "done".to_owned(),
            turns: 1,
            usage: TokenUsage::default(),
        };
        let receipt = AgentExecutionReceipt::new(
            "exec_test",
            AgentExecutionStatus::Completed,
            "done",
        )
        .with_step_ids(vec!["execstep_1".to_owned()])
        .with_terminal_projection(
            AgentExecutionSummary {
                step_count: 1,
                completed_count: 1,
                failed_count: 0,
                usage: TokenUsage::default(),
            },
            vec![result],
            None,
        );
        let value = serde_json::to_value(receipt).unwrap();
        assert_eq!(value["step_ids"][0], "execstep_1");
        assert_eq!(value["results"][0]["status"], "completed");
        assert_eq!(value["summary"]["completed_count"], 1);
    }
}

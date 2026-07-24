use std::sync::Arc;

use async_trait::async_trait;
use nomifun_common::AgentExecutionId;
use serde_json::{Value, json};

use crate::local_agent_invocation::LocalAgentInvocationRunner;
use nomi_protocol::events::ToolCategory;
use nomi_tools::Tool;
use nomi_types::agent::{
    AgentDelegationTask, AgentExecutionReceipt, AgentExecutionStatus, AgentExecutionStepResult,
    AgentExecutionSummary, AgentInvocationInput, AgentInvocationOutput, AgentToolPolicy,
    ParallelDelegationRequest, apply_agent_role_context,
};
use nomi_types::message::TokenUsage;
use nomi_types::tool::{JsonSchema, ToolResult};

const DEFAULT_AGENT_MAX_TURNS: usize = 200;
const DEFAULT_AGENT_MAX_TOKENS: u32 = 4096;

const DESCRIPTION: &str = concat!(
    "Start one embedded Agent Execution with strategy=parallel. It synchronously invokes ",
    "1-16 Agents (200 turns and 4096 output tokens each) and returns the same ",
    "execution_id/status/message receipt as a platform execution, plus terminal results. ",
    "Sibling progress is coordinated by the host without adding another model tool. ",
    "Use synthesize=true to add a read-only consolidation pass. Dependency DAGs and ",
    "durable cross-turn recovery require a platform host."
);

fn local_delegate_json_schema() -> JsonSchema {
    serde_json::to_value(schemars::schema_for!(ParallelDelegationRequest))
        .expect("embedded delegation schema is serializable")
}

/// Embedded deployment of the shared `nomi_delegate` Agent Execution request.
///
/// It projects the execution synchronously because a CLI host has no durable
/// scheduler, but uses the same request, receipt and lifecycle vocabulary.
pub(crate) struct LocalDelegateTool {
    runner: Arc<LocalAgentInvocationRunner>,
}

impl LocalDelegateTool {
    pub(crate) fn new(runner: Arc<LocalAgentInvocationRunner>) -> Self {
        Self { runner }
    }
}

#[async_trait]
impl Tool for LocalDelegateTool {
    fn name(&self) -> &str {
        "nomi_delegate"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> JsonSchema {
        local_delegate_json_schema()
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    fn is_deferred(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let request = match parse_request(&input) {
            Ok(request) => request,
            Err(error) => return rejected(error),
        };

        let execution_id = embedded_execution_id();
        let synthesize = request.synthesize;
        let invocations = request
            .tasks
            .into_iter()
            .map(task_invocation)
            .collect::<Vec<_>>();
        let results = self.runner.execute_fanout(invocations).await;

        let synthesis = if synthesize && results.len() >= 2 {
            Some(
                self.runner
                    .invoke_one(AgentInvocationInput {
                        name: "synthesizer".to_owned(),
                        prompt: build_synthesis_prompt(&results),
                        max_turns: DEFAULT_AGENT_MAX_TURNS,
                        max_tokens: DEFAULT_AGENT_MAX_TOKENS,
                        system_prompt: None,
                        model: None,
                        effort: None,
                        tool_policy: AgentToolPolicy::ReadOnly,
                        exact_tools: Vec::new(),
                    })
                    .await,
            )
        } else {
            None
        };

        completed(execution_id, &results, synthesis.as_ref())
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Exec
    }

    fn describe(&self, input: &Value) -> String {
        let tasks = input.get("tasks").and_then(Value::as_array);
        let task_count = tasks.map(Vec::len).unwrap_or(0);
        if task_count == 1 {
            let prompt = tasks
                .and_then(|tasks| tasks.first())
                .and_then(|task| task.get("prompt").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| "invoked Agent".to_owned());
            format!("Delegate: {}", nomi_tools::truncate_utf8(&prompt, 80))
        } else {
            format!("Delegate to {task_count} Agents")
        }
    }
}

fn parse_request(input: &Value) -> Result<ParallelDelegationRequest, String> {
    let request = serde_json::from_value::<ParallelDelegationRequest>(input.clone())
        .map_err(|error| format!("invalid nomi_delegate request: {error}"))?;
    request.validate()?;
    Ok(request)
}

fn embedded_execution_id() -> String {
    AgentExecutionId::new().into_string()
}

fn task_invocation(task: AgentDelegationTask) -> AgentInvocationInput {
    let AgentDelegationTask {
        name,
        prompt,
        role,
        tool_policy,
    } = task;
    AgentInvocationInput {
        name,
        prompt: apply_agent_role_context(prompt, role.as_deref()),
        max_turns: DEFAULT_AGENT_MAX_TURNS,
        max_tokens: DEFAULT_AGENT_MAX_TOKENS,
        system_prompt: None,
        model: None,
        effort: None,
        tool_policy,
        exact_tools: Vec::new(),
    }
}

fn build_synthesis_prompt(results: &[AgentInvocationOutput]) -> String {
    let mut digest = String::from(
        concat!(
            "Several Agents worked on parts of one task. Consolidate every reliable output into one ",
            "coherent answer. Explicitly flag conflicts or disagreements. Treat [ERROR] outputs as ",
            "unreliable, state the gap they leave, and verify load-bearing claims with read-only ",
            "workspace tools when practical.\n\n--- Agent outputs ---\n"
        ),
    );
    for result in results {
        let status = if result.is_error { "ERROR" } else { "OK" };
        digest.push_str(&format!(
            "\n### {} [{}]\n{}\n",
            result.name, status, result.text
        ));
    }
    digest.push_str(
        concat!(
            "\n--- End Agent outputs ---\nProduce the consolidated answer followed by a short ",
            "Conflicts/Gaps section; write 'none' if there are none."
        ),
    );
    digest
}

fn completed(
    execution_id: String,
    results: &[AgentInvocationOutput],
    synthesis: Option<&AgentInvocationOutput>,
) -> ToolResult {
    let ok_count = results.iter().filter(|result| !result.is_error).count();
    let error_count = results.len() - ok_count;
    let synthesis_error = synthesis.is_some_and(|result| result.is_error);
    let status = if ok_count == 0 {
        AgentExecutionStatus::Failed
    } else if error_count > 0 || synthesis_error {
        AgentExecutionStatus::CompletedWithFailures
    } else {
        AgentExecutionStatus::Completed
    };
    let message = match status {
        AgentExecutionStatus::Completed => {
            "Delegated work completed in this embedded Agent Execution."
        }
        AgentExecutionStatus::CompletedWithFailures => {
            "Delegated work completed with one or more failed Agent tasks."
        }
        AgentExecutionStatus::Failed => {
            "Delegated work failed because every Agent task failed."
        }
        _ => unreachable!("embedded execution returns only terminal projections"),
    };
    let all_outputs = results.iter().chain(synthesis);
    let usage = all_outputs.clone().fold(TokenUsage::default(), |mut total, output| {
        total.input_tokens += output.usage.input_tokens;
        total.output_tokens += output.usage.output_tokens;
        total.cache_creation_tokens += output.usage.cache_creation_tokens;
        total.cache_read_tokens += output.usage.cache_read_tokens;
        total
    });
    let summary = AgentExecutionSummary {
        step_count: results.len() + usize::from(synthesis.is_some()),
        completed_count: all_outputs.clone().filter(|output| !output.is_error).count(),
        failed_count: all_outputs.filter(|output| output.is_error).count(),
        usage,
    };
    let receipt = AgentExecutionReceipt::new(execution_id, status, message)
        .with_terminal_projection(
            summary,
            results.iter().map(AgentExecutionStepResult::from).collect(),
            synthesis.map(AgentExecutionStepResult::from),
        );
    let payload = json!({"result": receipt});
    ToolResult {
        content: serde_json::to_string(&payload).expect("execution projection is serializable"),
        // A terminal failed execution is still a valid receipt. Only a rejected
        // request is a tool-protocol error.
        is_error: false,
        images: Vec::new(),
    }
}

fn rejected(error: String) -> ToolResult {
    ToolResult {
        content: serde_json::to_string(&json!({"error": error}))
            .expect("delegation rejection is serializable"),
        is_error: true,
        images: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DESCRIPTION, build_synthesis_prompt, completed, embedded_execution_id,
        local_delegate_json_schema, parse_request, rejected, task_invocation,
    };
    use nomifun_common::AgentExecutionId;
    use nomi_types::agent::{AgentDelegationTask, AgentInvocationOutput, AgentToolPolicy};
    use nomi_types::message::TokenUsage;
    use serde_json::{Value, json};

    fn output(name: &str, text: &str, is_error: bool) -> AgentInvocationOutput {
        AgentInvocationOutput {
            name: name.to_owned(),
            text: text.to_owned(),
            usage: TokenUsage::default(),
            turns: 1,
            is_error,
        }
    }

    #[test]
    fn description_exposes_one_execution_contract_and_capacity() {
        for required in ["16", "parallel", "200", "4096", "execution_id/status/message"] {
            assert!(DESCRIPTION.contains(required), "missing {required}: {DESCRIPTION}");
        }
        assert!(!DESCRIPTION.contains(&["local", "immediate"].join("_")));
        assert!(!DESCRIPTION.contains(&["persistent", "execution"].join("_")));
        assert!(!DESCRIPTION.contains("execution mode"));
    }

    #[test]
    fn shared_task_accepts_free_form_role_and_explicit_policy() {
        let request = parse_request(&json!({
            "strategy": "parallel",
            "tasks": [
                {"name":"build","prompt":"implement","role":"builder"},
                {"name":"领域","prompt":"检查","role":"领域专家","tool_policy":"read_only"}
            ]
        }))
        .unwrap();
        assert_eq!(request.tasks[0].tool_policy, AgentToolPolicy::Full);
        assert_eq!(request.tasks[1].role.as_deref(), Some("领域专家"));
        assert_eq!(request.tasks[1].tool_policy, AgentToolPolicy::ReadOnly);
    }

    #[test]
    fn local_role_context_reaches_the_agent_prompt_without_changing_authority() {
        let invocation = task_invocation(AgentDelegationTask {
            name: "review".to_owned(),
            prompt: "Inspect the database migration.".to_owned(),
            role: Some("database reviewer".to_owned()),
            tool_policy: AgentToolPolicy::ReadOnly,
        });
        assert!(
            invocation
                .prompt
                .starts_with("DELEGATED ROLE CONTEXT: database reviewer\n")
        );
        assert!(invocation.prompt.ends_with("Inspect the database migration."));
        assert_eq!(invocation.tool_policy, AgentToolPolicy::ReadOnly);
    }

    #[test]
    fn tasks_must_be_a_json_array() {
        assert!(parse_request(&json!({
            "strategy": "parallel",
            "tasks": "[{\"name\":\"A\",\"prompt\":\"inspect\",\"tool_policy\":\"read_shell\"}]"
        }))
        .is_err());
    }

    #[test]
    fn local_schema_refs_resolve_from_the_single_root() {
        fn collect_refs<'a>(value: &'a Value, refs: &mut Vec<&'a str>) {
            match value {
                Value::Object(object) => {
                    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                        refs.push(reference);
                    }
                    for value in object.values() {
                        collect_refs(value, refs);
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        collect_refs(value, refs);
                    }
                }
                _ => {}
            }
        }

        let schema = local_delegate_json_schema();
        let mut refs = Vec::new();
        collect_refs(&schema, &mut refs);
        assert!(!refs.is_empty(), "shared task types should use root definitions");
        for reference in refs {
            let pointer = reference
                .strip_prefix('#')
                .expect("local schema references must stay inside the root document");
            assert!(
                schema.pointer(pointer).is_some(),
                "unresolved root schema reference: {reference}"
            );
        }
    }

    #[test]
    fn local_request_uses_shared_blank_task_validation() {
        for task in [
            json!({"name":" ","prompt":"inspect"}),
            json!({"name":"A","prompt":"\n"}),
            json!({"name":"A","prompt":"inspect","role":"\t"}),
        ] {
            assert!(parse_request(&json!({
                "strategy": "parallel",
                "tasks": [task]
            }))
            .is_err());
        }
    }

    #[test]
    fn model_cannot_choose_host_execution_mode_or_unknown_policy() {
        assert!(parse_request(&json!({
            "strategy":"parallel",
            "tasks":[{"name":"A","prompt":"a"}],
            "coordinate":true
        }))
        .is_err());
        assert!(parse_request(&json!({
            "strategy":"parallel",
            "tasks":[{"name":"A","prompt":"a"}],
            "isolate":true
        }))
        .is_err());
        assert!(parse_request(&json!({
            "strategy":"parallel",
            "tasks":[{"name":"A","prompt":"a","tool_policy":"admin"}]
        }))
        .is_err());
    }

    #[test]
    fn embedded_result_uses_the_canonical_execution_receipt() {
        let execution_id = AgentExecutionId::new().into_string();
        let result = completed(execution_id.clone(), &[output("A", "done", false)], None);
        let payload: Value = serde_json::from_str(&result.content).unwrap();
        let receipt = &payload["result"];
        assert_eq!(receipt["execution_id"], execution_id);
        assert_eq!(receipt["status"], "completed");
        assert!(receipt["message"].as_str().unwrap().contains("completed"));
        assert_eq!(receipt["results"][0]["name"], "A");
        assert!(receipt.get("mode").is_none());
        assert!(receipt.get("execution_mode").is_none());
        assert!(!result.is_error);
    }

    #[test]
    fn embedded_terminal_statuses_match_agent_execution_wire_values() {
        let partial_id = AgentExecutionId::new().into_string();
        let partial = completed(
            partial_id,
            &[output("A", "done", false), output("B", "failed", true)],
            None,
        );
        let partial: Value = serde_json::from_str(&partial.content).unwrap();
        assert_eq!(partial["result"]["status"], "completed_with_failures");

        let failed_id = AgentExecutionId::new().into_string();
        let failed = completed(
            failed_id,
            &[output("A", "failed", true)],
            None,
        );
        assert!(!failed.is_error, "a failed execution is still a valid receipt");
        let failed: Value = serde_json::from_str(&failed.content).unwrap();
        assert_eq!(failed["result"]["status"], "failed");
    }

    #[test]
    fn rejected_request_is_a_tool_error_not_a_fake_lifecycle_status() {
        let result = rejected("invalid request".to_owned());
        assert!(result.is_error);
        let payload: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(payload["error"], "invalid request");
        assert!(payload.get("status").is_none());
        assert!(payload.get("execution_id").is_none());
    }

    #[test]
    fn embedded_execution_ids_are_canonical_bare_uuidv7() {
        let id = embedded_execution_id();
        assert_eq!(id.len(), nomifun_common::UUID_STRING_LEN);
        assert!(nomifun_common::validate_uuidv7(&id).is_ok());
        assert!(!id.contains('_'));
    }

    #[test]
    fn synthesis_prompt_contains_every_status_and_conflict_instruction() {
        let prompt = build_synthesis_prompt(&[
            output("scan", "found issue", false),
            output("verify", "timed out", true),
        ]);
        assert!(prompt.contains("scan"));
        assert!(prompt.contains("verify"));
        assert!(prompt.contains("ERROR"));
        assert!(prompt.to_ascii_lowercase().contains("conflict"));
    }
}

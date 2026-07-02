use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::spawner::{AgentSpawner, SubAgentConfig};
use nomi_protocol::events::ToolCategory;
use nomi_types::tool::{JsonSchema, ToolResult};

use nomi_tools::Tool;

const DEFAULT_SUB_AGENT_MAX_TURNS: usize = 200;
const DEFAULT_SUB_AGENT_MAX_TOKENS: u32 = 4096;
const MAX_SUB_AGENTS: usize = 5;

pub struct SpawnTool {
    spawner: Arc<AgentSpawner>,
}

impl SpawnTool {
    pub fn new(spawner: Arc<AgentSpawner>) -> Self {
        Self { spawner }
    }
}

#[async_trait]
impl Tool for SpawnTool {
    fn name(&self) -> &str {
        "Spawn"
    }

    fn description(&self) -> &str {
        "Spawn one or more sub-agents to handle tasks in parallel. \
         Each sub-agent has its own conversation context and tool access.\n\n\
         - Maximum 5 sub-agents per call.\n\
         - Each sub-agent runs up to 200 conversation turns with a 4096 token output limit.\n\
         - Use for independent, parallelizable tasks (e.g., searching different modules, \
         running separate analyses).\n\
         - Do NOT use for tasks that need shared state or sequential coordination.\n\
         - Set `synthesize: true` to add a final pass that consolidates the sub-agents' \
         outputs and flags conflicts (useful for verification / judge-panel fan-outs)."
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "description": "List of tasks for sub-agents to execute in parallel",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Short descriptive name for the task"
                            },
                            "prompt": {
                                "type": "string",
                                "description": "The task description / prompt for the sub-agent"
                            },
                            "role": {
                                "type": "string",
                                "enum": ["searcher", "reviewer", "verifier", "implementer"],
                                "description": "Optional role restricting the sub-agent's tools: searcher/reviewer = read-only (Read/Grep/Glob), verifier = read-only + Bash, implementer (default) = all tools."
                            }
                        },
                        "required": ["name", "prompt"]
                    }
                },
                "synthesize": {
                    "type": "boolean",
                    "description": "When true (and ≥2 tasks), run one extra read-only sub-agent that consolidates all sub-agent outputs into a single answer and flags any conflicts. Use for verification/judge-panel fan-outs where the results must be reconciled; omit for independent tasks you will combine yourself."
                },
                "coordinate": {
                    "type": "boolean",
                    "description": "When true, all sub-agents share a task board (the `shared_tasks` tool): they can claim work to avoid duplicating it and report progress to each other. Use when the tasks overlap or must divide a work-list; omit for fully independent tasks."
                },
                "isolate": {
                    "type": "boolean",
                    "description": "When true, each sub-agent runs in its own isolated git worktree so concurrent edits don't clobber the shared tree. Each result includes its changes as a diff for you to review and apply with ApplyPatch. Use for parallel implementers that edit files; requires a git workspace."
                }
            },
            "required": ["tasks"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false // manages its own concurrency
    }

    fn is_deferred(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let tasks = match parse_tasks(&input) {
            Ok(tasks) => tasks,
            Err(e) => {
                return ToolResult {
                    content: e,
                    is_error: true,
                    images: Vec::new(),
                };
            }
        };

        if tasks.is_empty() {
            return ToolResult {
                content: "No tasks provided".to_string(),
                is_error: true,
                images: Vec::new(),
            };
        }

        if tasks.len() > MAX_SUB_AGENTS {
            return ToolResult {
                content: format!(
                    "Too many sub-agents: {} (max {})",
                    tasks.len(),
                    MAX_SUB_AGENTS
                ),
                is_error: true,
                images: Vec::new(),
            };
        }

        let results = if input.get("isolate").and_then(|v| v.as_bool()).unwrap_or(false) {
            // Isolated fan-out: each sub-agent edits in its own git worktree (§3.4).
            self.spawner.spawn_parallel_isolated(tasks).await
        } else if input.get("coordinate").and_then(|v| v.as_bool()).unwrap_or(false)
            && tasks.len() >= 2
        {
            // Coordinated fan-out: siblings share a task board (TIER 2 §3.4).
            self.spawner.spawn_parallel_coordinated(tasks).await
        } else {
            self.spawner.spawn_parallel(tasks).await
        };

        let output: Vec<String> = results
            .iter()
            .map(|r| {
                let status = if r.is_error { "ERROR" } else { "OK" };
                format!(
                    "## {} [{}]\n{}\n[turns: {} | tokens: {} in / {} out]",
                    r.name, status, r.text, r.turns, r.usage.input_tokens, r.usage.output_tokens
                )
            })
            .collect();

        let all_error = results.iter().all(|r| r.is_error);

        // Aggregate header so the parent (and user) see the overall outcome and
        // the total token cost across the fan-out at a glance.
        let ok_count = results.iter().filter(|r| !r.is_error).count();
        let err_count = results.len() - ok_count;
        let total_in: u64 = results.iter().map(|r| r.usage.input_tokens).sum();
        let total_out: u64 = results.iter().map(|r| r.usage.output_tokens).sum();
        let header = format!(
            "{} sub-agent(s): {} ok, {} error(s) | total {} in / {} out tokens",
            results.len(),
            ok_count,
            err_count,
            total_in,
            total_out
        );

        let fan_out = format!("{}\n\n{}", header, output.join("\n\n---\n\n"));

        // Optional synthesis pass (§3.4 verify/synthesize): consolidate the
        // sub-agent outputs into one answer and flag conflicts. Opt-in and only
        // when there is more than one result to combine; a synthesis failure
        // falls back to the raw fan-out so the parent never loses the results.
        if wants_synthesis(&input, results.len()) {
            let synth_config = SubAgentConfig {
                name: "synthesizer".to_string(),
                prompt: build_synthesis_prompt(&results),
                max_turns: DEFAULT_SUB_AGENT_MAX_TURNS,
                max_tokens: DEFAULT_SUB_AGENT_MAX_TOKENS,
                system_prompt: None,
                // Read-only: the synthesizer reasons over the outputs and may
                // verify claims against the workspace, but must not mutate.
                allowed_tools: role_tools(Some("reviewer")),
            };
            let synth = self.spawner.spawn_one(synth_config).await;
            if !synth.is_error {
                return ToolResult {
                    content: format!(
                        "{}\n\n=== Synthesis ===\n{}\n\n<details: raw sub-agent outputs below>\n\n{}",
                        header, synth.text, output.join("\n\n---\n\n")
                    ),
                    is_error: false,
                    images: Vec::new(),
                };
            }
            // Synthesis failed — fall through to the raw fan-out.
        }

        ToolResult {
            content: fan_out,
            is_error: all_error,
            images: Vec::new(),
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Exec
    }

    fn describe(&self, input: &Value) -> String {
        // The schema field is `tasks` (an array); the old code read a
        // non-existent `task` and always fell through to the generic default.
        let n = input
            .get("tasks")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        if n == 1 {
            // Surface the single task's prompt for a useful label.
            let prompt = input["tasks"][0]
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("sub-agent");
            format!("Spawn: {}", nomi_tools::truncate_utf8(prompt, 80))
        } else {
            format!("Spawn {} parallel sub-agents", n)
        }
    }
}

/// Map an optional sub-agent role to a restricted tool whitelist. An empty
/// result (default / "implementer") means all built-in tools. Names match the
/// tools registered in `spawner::build_tool_registry`.
fn role_tools(role: Option<&str>) -> Vec<String> {
    let names: &[&str] = match role.map(|r| r.to_ascii_lowercase()).as_deref() {
        Some("searcher" | "scout" | "reviewer") => &["Read", "Grep", "Glob"],
        Some("verifier" | "tester") => &["Read", "Grep", "Glob", "Bash"],
        // "implementer", unknown, or absent -> full toolset.
        _ => &[],
    };
    names.iter().map(|s| s.to_string()).collect()
}

/// Whether the caller opted into a post-fan-out synthesis pass. Opt-in (default
/// off so existing fan-outs are unchanged) and only meaningful with ≥2 results
/// to consolidate.
fn wants_synthesis(input: &Value, result_count: usize) -> bool {
    input
        .get("synthesize")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        && result_count >= 2
}

/// Build the synthesizer sub-agent's prompt from the fan-out results: a digest
/// that delineates each sub-agent's output and status, and instructs the
/// synthesizer to consolidate them into one answer while explicitly flagging any
/// conflicts/disagreements between sub-agents. Pure (no engine) so it is
/// unit-testable.
fn build_synthesis_prompt(results: &[crate::spawner::SubAgentResult]) -> String {
    let mut digest = String::from(
        "You are a synthesis agent. Several sub-agents independently worked on \
         parts of one task; their outputs are below. Consolidate them into a \
         single coherent answer. Explicitly flag any CONFLICTS or disagreements \
         between sub-agents (e.g. contradictory findings or conclusions) rather \
         than silently picking one. Treat outputs marked [ERROR] as unreliable — \
         note the gap they leave. Verify load-bearing claims against the \
         workspace with your read-only tools when practical.\n\n\
         --- Sub-agent outputs ---\n",
    );
    for r in results {
        let status = if r.is_error { "ERROR" } else { "OK" };
        digest.push_str(&format!("\n### {} [{}]\n{}\n", r.name, status, r.text));
    }
    digest.push_str(
        "\n--- End of sub-agent outputs ---\n\n\
         Produce: (1) the consolidated answer, and (2) a short \"Conflicts/Gaps\" \
         section (write \"none\" if there are none).",
    );
    digest
}

fn parse_tasks(input: &Value) -> Result<Vec<SubAgentConfig>, String> {
    // Accept both a real array and a provider-stringified one (`tasks` sent as a
    // JSON string). The engine also coerces this centrally
    // (nomi_tools::coerce_input_to_schema) before dispatch; this local net keeps
    // the tool correct when called directly (sub-agent runners, tests).
    let tasks_arr: Vec<Value> = match &input["tasks"] {
        Value::Array(a) => a.clone(),
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .and_then(|v| match v {
                Value::Array(a) => Some(a),
                _ => None,
            })
            .ok_or("Missing or invalid 'tasks' array")?,
        _ => return Err("Missing or invalid 'tasks' array".to_string()),
    };

    let mut configs = Vec::new();
    for task in &tasks_arr {
        let name = task["name"]
            .as_str()
            .ok_or("Each task must have a 'name' string")?
            .to_string();
        let prompt = task["prompt"]
            .as_str()
            .ok_or("Each task must have a 'prompt' string")?
            .to_string();

        configs.push(SubAgentConfig {
            name,
            prompt,
            max_turns: DEFAULT_SUB_AGENT_MAX_TURNS,
            max_tokens: DEFAULT_SUB_AGENT_MAX_TOKENS,
            system_prompt: None,
            allowed_tools: role_tools(task["role"].as_str()),
        });
    }

    Ok(configs)
}

#[cfg(test)]
mod tests {
    use super::{build_synthesis_prompt, parse_tasks, role_tools, wants_synthesis};
    use crate::spawner::SubAgentResult;
    use nomi_types::message::TokenUsage;
    use serde_json::json;

    #[test]
    fn parse_tasks_accepts_real_array() {
        let cfgs = parse_tasks(&json!({
            "tasks": [{"name": "a", "prompt": "p1"}, {"name": "b", "prompt": "p2", "role": "searcher"}]
        }))
        .unwrap();
        assert_eq!(cfgs.len(), 2);
        assert_eq!(cfgs[0].name, "a");
        assert_eq!(cfgs[0].prompt, "p1");
        // role=searcher restricts tools to read-only.
        assert_eq!(cfgs[1].allowed_tools, vec!["Read", "Grep", "Glob"]);
    }

    #[test]
    fn parse_tasks_accepts_provider_stringified_array() {
        // The reported failure: `tasks` arrived as a JSON *string*. It must parse.
        let cfgs = parse_tasks(&json!({
            "tasks": "[{\"name\": \"子agent1\", \"prompt\": \"请直接输出: hello world\"}, {\"name\": \"子agent2\", \"prompt\": \"请直接输出: hello world\"}]"
        }))
        .unwrap();
        assert_eq!(cfgs.len(), 2);
        assert_eq!(cfgs[0].name, "子agent1");
        assert_eq!(cfgs[1].prompt, "请直接输出: hello world");
    }

    #[test]
    fn parse_tasks_rejects_missing_and_garbage() {
        assert!(parse_tasks(&json!({})).is_err());
        assert!(parse_tasks(&json!({ "tasks": "not json" })).is_err());
        assert!(parse_tasks(&json!({ "tasks": 42 })).is_err());
        // Missing required per-task fields still error clearly.
        assert!(parse_tasks(&json!({ "tasks": [{"name": "a"}] })).is_err());
    }

    fn result(name: &str, text: &str, is_error: bool) -> SubAgentResult {
        SubAgentResult {
            name: name.to_string(),
            text: text.to_string(),
            usage: TokenUsage::default(),
            turns: 1,
            is_error,
        }
    }

    #[test]
    fn role_tools_maps_roles_to_whitelists() {
        assert_eq!(role_tools(Some("searcher")), vec!["Read", "Grep", "Glob"]);
        // case-insensitive
        assert_eq!(role_tools(Some("Reviewer")), vec!["Read", "Grep", "Glob"]);
        assert_eq!(
            role_tools(Some("verifier")),
            vec!["Read", "Grep", "Glob", "Bash"]
        );
        // implementer / unknown / absent -> full toolset (empty whitelist)
        assert!(role_tools(Some("implementer")).is_empty());
        assert!(role_tools(None).is_empty());
        assert!(role_tools(Some("nonsense")).is_empty());
    }

    #[test]
    fn wants_synthesis_is_opt_in_and_needs_two_results() {
        // Absent / false → no synthesis (back-compat: existing fan-outs unchanged).
        assert!(!wants_synthesis(&json!({"tasks": []}), 3));
        assert!(!wants_synthesis(&json!({"synthesize": false}), 3));
        // Enabled but <2 results → nothing to synthesize.
        assert!(!wants_synthesis(&json!({"synthesize": true}), 1));
        assert!(!wants_synthesis(&json!({"synthesize": true}), 0));
        // Enabled with ≥2 results → synthesize.
        assert!(wants_synthesis(&json!({"synthesize": true}), 2));
        assert!(wants_synthesis(&json!({"synthesize": true}), 5));
    }

    #[test]
    fn build_synthesis_prompt_includes_results_status_and_conflict_instruction() {
        let results = vec![
            result("scan-auth", "found a bug in login.rs:42", false),
            result("scan-db", "no issues in the db layer", false),
            result("scan-net", "timed out", true),
        ];
        let prompt = build_synthesis_prompt(&results);

        // Each sub-agent's name and text must appear so the synthesizer can reason
        // over all of them.
        assert!(prompt.contains("scan-auth"));
        assert!(prompt.contains("login.rs:42"));
        assert!(prompt.contains("scan-db"));
        assert!(prompt.contains("scan-net"));
        // Failed sub-agents are marked so the synthesizer weights them accordingly.
        assert!(prompt.contains("ERROR") || prompt.contains("FAILED"));
        // The synthesizer is explicitly told to consolidate AND flag conflicts.
        let lower = prompt.to_lowercase();
        assert!(lower.contains("conflict") || lower.contains("disagree"));
        assert!(lower.contains("consolidat") || lower.contains("synthesiz") || lower.contains("combine"));
    }
}

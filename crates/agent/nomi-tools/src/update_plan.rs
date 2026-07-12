use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use nomi_protocol::events::ToolCategory;
use nomi_types::tool::{JsonSchema, ToolResult};

use crate::Tool;

const INCOMPLETE_PLAN_REMINDER: &str = "\
[progress] Plan updated. Send the next full snapshot only when a meaningful milestone changes \
state, scope changes, a blocker appears, or final verification completes. Before final response, \
run or record verification and mark every real milestone completed.\n";

const MISSING_VERIFICATION_REMINDER: &str = "\
[verification] No explicit verification step is present in the completed plan. If this task changed code, files, data, or user-visible behavior, run the narrowest meaningful verification and update the plan before final response.\n";

/// Single step status. snake_case aligns with codex and the frontend
/// `entry.status` (`pending`/`in_progress`/`completed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
}

/// One plan step (argument). `step` is the step text; it is normalized to
/// `content` for the frontend plan renderer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanItemArg {
    pub step: String,
    pub status: StepStatus,
}

/// `update_plan` arguments — a stateless full snapshot of the plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatePlanArgs {
    #[serde(default)]
    pub explanation: Option<String>,
    pub plan: Vec<PlanItemArg>,
}

/// codex-style todo/checklist tool. Stateless: the model submits the full step
/// list every call.
///
/// This is a different concept from nomi's Plan Mode
/// (`EnterPlanMode`/`ExitPlanMode`), which is a *mode* that restricts the tool
/// allow-list. `update_plan` is a *progress declaration* tool.
pub struct UpdatePlanTool;

impl UpdatePlanTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for UpdatePlanTool {
    fn default() -> Self {
        Self::new()
    }
}

fn step_mentions_verification(step: &str) -> bool {
    let lower = step.to_lowercase();
    ["verify", "verification", "test", "typecheck", "build", "compile"]
        .iter()
        .any(|needle| lower.contains(needle))
        || ["验证", "测试", "检查", "编译", "构建"]
            .iter()
            .any(|needle| step.contains(needle))
}

#[async_trait]
impl Tool for UpdatePlanTool {
    fn name(&self) -> &str {
        "update_plan"
    }

    fn description(&self) -> &str {
        "Update the task plan (a todo checklist shown to the user). \
         Provide an optional `explanation` and a full `plan`: the complete list of steps, \
         each with a one-line `step` and a `status` of pending, in_progress, or completed. \
         This is a stateless full snapshot — send the entire current plan every time, not a diff. \
         There should be exactly one in_progress step until all are completed; mark a step \
         completed before starting the next. Use it for non-trivial multi-step work; do not use \
         it for simple single-step queries, and do not pad with filler steps. Each step should be \
         a meaningful milestone, not an individual tool call or internal sub-step. Do not send an \
         unchanged snapshot or one that changes only the explanation. At a transition, use one \
         snapshot that marks the previous milestone completed and the next milestone in_progress. \
         Before the final response send a full snapshot where every real step is completed. \
         For code/file/user-visible changes, include and complete a verification step before \
         finalizing. After calling it, do not repeat the full plan in your reply — just note \
         what changed and the next step."
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "explanation": {
                    "type": "string",
                    "description": "Optional rationale for this plan update."
                },
                "plan": {
                    "type": "array",
                    "description": "The full list of plan steps (complete snapshot).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "step":   { "type": "string", "description": "Task step text (one short line)." },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Step status."
                            }
                        },
                        "required": ["step", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["plan"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        // Pure declaration, no side effects — safe to run concurrently.
        true
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Info
    }

    async fn execute(&self, input: Value) -> ToolResult {
        // 1) Parse arguments.
        let args: UpdatePlanArgs = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => {
                return ToolResult::error(format!("update_plan: invalid arguments: {e}"));
            }
        };

        if args.plan.is_empty() {
            return ToolResult::error("update_plan: `plan` must contain at least one step.");
        }

        // 2) Soft constraint: at most one in_progress. More than one does NOT
        //    fail (avoids breaking the agent loop, matching codex) — we just
        //    warn so the model self-corrects.
        let in_progress = args
            .plan
            .iter()
            .filter(|p| p.status == StepStatus::InProgress)
            .count();

        // 3) Normalize into frontend entry shape: { content, status }
        //    (note step -> content).
        let entries: Vec<Value> = args
            .plan
            .iter()
            .map(|p| {
                json!({
                    "content": p.step,
                    "status": match p.status {
                        StepStatus::Pending => "pending",
                        StepStatus::InProgress => "in_progress",
                        StepStatus::Completed => "completed",
                    }
                })
            })
            .collect();

        // 4) Encode the structured snapshot into content (JSON). The backend
        //    bridge layer parses this to emit a Plan event; the same string is
        //    the (compact) tool_result returned to the model.
        let payload = json!({
            "kind": "plan_update",
            "explanation": args.explanation,
            "entries": entries,
        });
        let content = serde_json::to_string(&payload)
            .unwrap_or_else(|_| "{\"kind\":\"plan_update\",\"entries\":[]}".to_string());

        let mut prefix = String::new();
        if in_progress > 1 {
            prefix.push_str(&format!(
                "[note] {in_progress} steps are in_progress; convention is exactly one. Plan rendered as submitted.\n"
            ));
        }

        let all_completed = args.plan.iter().all(|p| p.status == StepStatus::Completed);
        if all_completed {
            let has_verification = args.plan.iter().any(|p| step_mentions_verification(&p.step));
            if !has_verification {
                prefix.push_str(MISSING_VERIFICATION_REMINDER);
            }
        } else {
            prefix.push_str(INCOMPLETE_PLAN_REMINDER);
        }

        ToolResult::text(format!("{prefix}{content}"))
    }

    fn describe(&self, input: &Value) -> String {
        let n = input
            .get("plan")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        format!("Update plan ({n} steps)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_and_category() {
        let t = UpdatePlanTool::new();
        assert_eq!(t.name(), "update_plan");
        assert!(matches!(t.category(), ToolCategory::Info));
    }

    #[test]
    fn schema_requires_plan() {
        let s = UpdatePlanTool::new().input_schema();
        let req = s["required"].as_array().unwrap();
        assert!(req.iter().any(|v| v == "plan"));
        let item_req = s["properties"]["plan"]["items"]["required"].as_array().unwrap();
        assert!(item_req.iter().any(|v| v == "step"));
        assert!(item_req.iter().any(|v| v == "status"));
    }

    #[test]
    fn description_limits_updates_to_material_milestone_transitions() {
        let tool = UpdatePlanTool::new();
        let description = tool.description();
        assert!(description.contains("meaningful milestone"));
        assert!(description.contains("individual tool call"));
        assert!(description.contains("unchanged snapshot"));
        assert!(description.contains("previous milestone completed"));
    }

    #[tokio::test]
    async fn execute_rejects_empty_plan() {
        let r = UpdatePlanTool::new().execute(json!({ "plan": [] })).await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn execute_rejects_bad_args() {
        let r = UpdatePlanTool::new().execute(json!({ "plan": "nope" })).await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn execute_normalizes_step_to_content() {
        let r = UpdatePlanTool::new()
            .execute(json!({
                "plan": [{ "step": "Read code", "status": "in_progress" }]
            }))
            .await;
        assert!(!r.is_error);
        let start = r.content.find('{').unwrap();
        let v: serde_json::Value = serde_json::from_str(&r.content[start..]).unwrap();
        assert_eq!(v["kind"], "plan_update");
        assert_eq!(v["entries"][0]["content"], "Read code");
        assert_eq!(v["entries"][0]["status"], "in_progress");
    }

    #[tokio::test]
    async fn execute_one_in_progress_no_warning() {
        let r = UpdatePlanTool::new()
            .execute(json!({
                "plan": [
                    { "step": "a", "status": "completed" },
                    { "step": "b", "status": "in_progress" },
                    { "step": "c", "status": "pending" }
                ]
            }))
            .await;
        assert!(!r.is_error);
        assert!(!r.content.contains("[note]"));
    }

    #[tokio::test]
    async fn execute_incomplete_plan_reminds_model_to_keep_progress_and_verify_before_final() {
        let r = UpdatePlanTool::new()
            .execute(json!({
                "plan": [
                    { "step": "Inspect", "status": "completed" },
                    { "step": "Implement", "status": "in_progress" },
                    { "step": "Verify", "status": "pending" }
                ]
            }))
            .await;
        assert!(!r.is_error);
        assert!(r.content.contains("[progress]"));
        assert!(r.content.contains("next full snapshot"));
        assert!(r.content.contains("Before final response"));
        assert!(r.content.contains("verification"));
        assert!(!r.content.contains("after each completed step"));
        assert!(r.content.contains("milestone"));
        let start = r.content.find('{').unwrap();
        let v: serde_json::Value = serde_json::from_str(&r.content[start..]).unwrap();
        assert_eq!(v["entries"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn execute_completed_plan_without_verification_warns_before_finalizing() {
        let r = UpdatePlanTool::new()
            .execute(json!({
                "plan": [
                    { "step": "Inspect", "status": "completed" },
                    { "step": "Implement", "status": "completed" }
                ]
            }))
            .await;
        assert!(!r.is_error);
        assert!(r.content.contains("[verification]"));
        assert!(r.content.contains("No explicit verification"));
        let start = r.content.find('{').unwrap();
        let v: serde_json::Value = serde_json::from_str(&r.content[start..]).unwrap();
        assert_eq!(v["kind"], "plan_update");
    }

    #[tokio::test]
    async fn execute_multi_in_progress_warns_but_succeeds() {
        let r = UpdatePlanTool::new()
            .execute(json!({
                "plan": [
                    { "step": "a", "status": "in_progress" },
                    { "step": "b", "status": "in_progress" }
                ]
            }))
            .await;
        assert!(!r.is_error);
        assert!(r.content.contains("[note]"));
        let start = r.content.find('{').unwrap();
        let v: serde_json::Value = serde_json::from_str(&r.content[start..]).unwrap();
        assert_eq!(v["entries"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn execute_carries_explanation() {
        let r = UpdatePlanTool::new()
            .execute(json!({
                "explanation": "re-scoping",
                "plan": [{ "step": "x", "status": "pending" }]
            }))
            .await;
        let start = r.content.find('{').unwrap();
        let v: serde_json::Value = serde_json::from_str(&r.content[start..]).unwrap();
        assert_eq!(v["explanation"], "re-scoping");
    }

    #[test]
    fn describe_reports_step_count() {
        let t = UpdatePlanTool::new();
        let d = t.describe(&json!({ "plan": [ {"step":"a","status":"pending"}, {"step":"b","status":"pending"} ] }));
        assert!(d.contains('2'));
    }
}

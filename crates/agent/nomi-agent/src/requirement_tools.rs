//! Native tools that let an in-process agent report requirement progress back
//! to the backend through a `RequirementSink` trait object. The backend injects
//! a concrete sink; standalone `nomi-cli` passes `None` and these are not registered.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use nomi_protocol::events::ToolCategory;
use nomi_tools::Tool;
use nomi_types::tool::{JsonSchema, ToolResult};
use nomifun_common::RequirementId;

/// Backend seam for requirement self-updates. Implemented by the backend over
/// its `RequirementService`; `nomi-agent` only depends on this trait.
#[async_trait]
pub trait RequirementSink: Send + Sync {
    /// Mark a requirement done with a completion note.
    async fn complete(&self, requirement_id: &str, note: &str) -> Result<(), String>;

    /// Update a requirement's status (`in_progress` | `done` | `failed`) with an optional note.
    async fn update_status(
        &self,
        requirement_id: &str,
        status: &str,
        note: Option<&str>,
    ) -> Result<(), String>;
}

/// `requirement_complete` — mark the current requirement done.
pub struct RequirementCompleteTool {
    sink: Arc<dyn RequirementSink>,
}

impl RequirementCompleteTool {
    pub fn new(sink: Arc<dyn RequirementSink>) -> Self {
        Self { sink }
    }
}

#[async_trait]
impl Tool for RequirementCompleteTool {
    fn name(&self) -> &str {
        "requirement_complete"
    }

    fn description(&self) -> &str {
        "Mark the current AutoWork requirement as done. Call this exactly once when you have \
         finished the requirement you were given. Provide a concise completion note describing \
         what you did. Do not pick the next requirement yourself — the platform will hand it to you."
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "pattern": "^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$",
                    "description": "The canonical bare UUIDv7 requirement id you were given in the AutoWork prompt"
                },
                "completion_note": {
                    "type": "string",
                    "description": "A concise description of what was accomplished"
                }
            },
            "required": ["id", "completion_note"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    fn is_deferred(&self) -> bool {
        // NOT deferred: the AutoWork prompt instructs the agent to call this
        // tool directly, so its full parameter schema must be visible up front.
        // A deferred stub would make the model call it with `{}` (missing `id`)
        // and only then be told to ToolSearch — the bug this fixes.
        false
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let id = match input.get("id").and_then(Value::as_str) {
            Some(id) if RequirementId::parse(id).is_ok() => id,
            _ => {
                return ToolResult {
                    content: "Missing or invalid canonical UUIDv7 'id'".to_string(),
                    is_error: true,
                    images: Vec::new(),
                };
            }
        };
        let note = input
            .get("completion_note")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match self.sink.complete(id, note).await {
            Ok(()) => ToolResult {
                content: format!("Requirement {id} marked done."),
                is_error: false,
                images: Vec::new(),
            },
            Err(e) => ToolResult {
                content: format!("Failed to complete requirement {id}: {e}"),
                is_error: true,
                images: Vec::new(),
            },
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Exec
    }
}

/// `requirement_update_status` — set status to in_progress | done | failed.
pub struct RequirementUpdateStatusTool {
    sink: Arc<dyn RequirementSink>,
}

impl RequirementUpdateStatusTool {
    pub fn new(sink: Arc<dyn RequirementSink>) -> Self {
        Self { sink }
    }
}

#[async_trait]
impl Tool for RequirementUpdateStatusTool {
    fn name(&self) -> &str {
        "requirement_update_status"
    }

    fn description(&self) -> &str {
        "Update the status of the current AutoWork requirement. Use status='failed' with a reason \
         if you cannot complete it, or status='done' when finished. Valid: in_progress, done, failed."
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "pattern": "^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$",
                    "description": "The canonical bare UUIDv7 requirement id"
                },
                "status": {
                    "type": "string",
                    "enum": ["in_progress", "done", "failed"],
                    "description": "New status"
                },
                "note": { "type": "string", "description": "Optional reason / note" }
            },
            "required": ["id", "status"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    fn is_deferred(&self) -> bool {
        // NOT deferred: the AutoWork prompt instructs the agent to call this
        // tool directly, so its full parameter schema must be visible up front.
        // A deferred stub would make the model call it with `{}` (missing `id`)
        // and only then be told to ToolSearch — the bug this fixes.
        false
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let id = match input.get("id").and_then(Value::as_str) {
            Some(id) if RequirementId::parse(id).is_ok() => id,
            _ => {
                return ToolResult {
                    content: "Missing or invalid canonical UUIDv7 'id'".to_string(),
                    is_error: true,
                    images: Vec::new(),
                };
            }
        };
        let status = match input.get("status").and_then(|v| v.as_str()) {
            Some(s @ ("in_progress" | "done" | "failed")) => s,
            _ => {
                return ToolResult {
                    content: "Invalid 'status' (expected in_progress|done|failed)".to_string(),
                    is_error: true,
                    images: Vec::new(),
                };
            }
        };
        let note = input.get("note").and_then(|v| v.as_str());
        match self.sink.update_status(id, status, note).await {
            Ok(()) => ToolResult {
                content: format!("Requirement {id} status set to {status}."),
                is_error: false,
                images: Vec::new(),
            },
            Err(e) => ToolResult {
                content: format!("Failed to update requirement {id}: {e}"),
                is_error: true,
                images: Vec::new(),
            },
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Exec
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeSink {
        completed: Mutex<Vec<(String, String)>>,
        statuses: Mutex<Vec<(String, String, Option<String>)>>,
        fail: bool,
    }

    #[async_trait]
    impl RequirementSink for FakeSink {
        async fn complete(&self, id: &str, note: &str) -> Result<(), String> {
            if self.fail {
                return Err("boom".into());
            }
            self.completed
                .lock()
                .unwrap()
                .push((id.to_string(), note.to_string()));
            Ok(())
        }
        async fn update_status(
            &self,
            id: &str,
            status: &str,
            note: Option<&str>,
        ) -> Result<(), String> {
            if self.fail {
                return Err("boom".into());
            }
            self.statuses.lock().unwrap().push((
                id.to_string(),
                status.to_string(),
                note.map(|s| s.to_string()),
            ));
            Ok(())
        }
    }

    #[tokio::test]
    async fn complete_calls_sink() {
        let sink = Arc::new(FakeSink::default());
        let tool = RequirementCompleteTool::new(sink.clone());
        let id = RequirementId::new().into_string();
        let res = tool
            .execute(json!({ "id": id, "completion_note": "done it" }))
            .await;
        assert!(!res.is_error, "content: {}", res.content);
        let completed = sink.completed.lock().unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0], (id, "done it".to_string()));
    }

    #[tokio::test]
    async fn complete_missing_id_is_error() {
        let sink = Arc::new(FakeSink::default());
        let tool = RequirementCompleteTool::new(sink);
        let res = tool.execute(json!({ "completion_note": "x" })).await;
        assert!(res.is_error);
    }

    #[tokio::test]
    async fn complete_rejects_non_uuid_string_id() {
        let sink = Arc::new(FakeSink::default());
        let tool = RequirementCompleteTool::new(sink);
        let res = tool
            .execute(json!({ "id": "1", "completion_note": "done it" }))
            .await;
        assert!(res.is_error);
    }

    #[tokio::test]
    async fn update_status_validates_enum() {
        let sink = Arc::new(FakeSink::default());
        let tool = RequirementUpdateStatusTool::new(sink.clone());
        let id = RequirementId::new().into_string();
        let bad = tool
            .execute(json!({ "id": id, "status": "weird" }))
            .await;
        assert!(bad.is_error);
        let good = tool
            .execute(json!({ "id": id, "status": "failed", "note": "blocked" }))
            .await;
        assert!(!good.is_error, "content: {}", good.content);
        let statuses = sink.statuses.lock().unwrap();
        assert_eq!(
            statuses[0],
            (
                id,
                "failed".to_string(),
                Some("blocked".to_string())
            )
        );
    }

    #[tokio::test]
    async fn sink_error_surfaces_as_tool_error() {
        let sink = Arc::new(FakeSink {
            fail: true,
            ..Default::default()
        });
        let tool = RequirementCompleteTool::new(sink);
        let id = RequirementId::new().into_string();
        let res = tool
            .execute(json!({ "id": id, "completion_note": "x" }))
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("boom"));
    }
}

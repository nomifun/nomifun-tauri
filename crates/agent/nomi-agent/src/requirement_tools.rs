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

fn claim_token(input: &Value) -> Option<&str> {
    input
        .get("claim_token")
        .and_then(Value::as_str)
        .filter(|token| {
            token.len() == 64
                && token
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

/// Backend seam for requirement self-updates. Implemented by the backend over
/// its `RequirementService`; `nomi-agent` only depends on this trait.
#[async_trait]
pub trait RequirementSink: Send + Sync {
    /// Mark a requirement done with a completion note.
    async fn complete(
        &self,
        owner_conversation_id: &str,
        requirement_id: &str,
        claim_generation: i64,
        claim_token: &str,
        note: &str,
    ) -> Result<(), String>;

    /// Update a requirement's status (`in_progress` | `done` | `failed`) with an optional note.
    async fn update_status(
        &self,
        owner_conversation_id: &str,
        requirement_id: &str,
        claim_generation: i64,
        claim_token: &str,
        status: &str,
        note: Option<&str>,
    ) -> Result<(), String>;
}

/// `requirement_complete` — mark the current requirement done.
pub struct RequirementCompleteTool {
    sink: Arc<dyn RequirementSink>,
    owner_conversation_id: String,
}

impl RequirementCompleteTool {
    pub fn new(
        sink: Arc<dyn RequirementSink>,
        owner_conversation_id: impl Into<String>,
    ) -> Self {
        Self {
            sink,
            owner_conversation_id: owner_conversation_id.into(),
        }
    }
}

#[async_trait]
impl Tool for RequirementCompleteTool {
    fn name(&self) -> &str {
        "requirement_complete"
    }

    fn description(&self) -> &str {
        "Mark the current AutoWork requirement as done. Call this exactly once when you have \
         finished the requirement you were given, using the exact claim_generation and \
         claim_token from the current prompt. Provide a concise completion note describing \
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
                "claim_generation": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "The exact positive claim generation from the current AutoWork prompt"
                },
                "claim_token": {
                    "type": "string",
                    "minLength": 64,
                    "maxLength": 64,
                    "pattern": "^[0-9a-f]{64}$",
                    "description": "The opaque claim token from the current AutoWork prompt"
                },
                "completion_note": {
                    "type": "string",
                    "description": "A concise description of what was accomplished"
                }
            },
            "required": ["id", "claim_generation", "claim_token", "completion_note"]
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
        let claim_generation = match input.get("claim_generation").and_then(Value::as_i64) {
            Some(generation) if generation > 0 => generation,
            _ => {
                return ToolResult {
                    content: "Missing or invalid positive integer 'claim_generation'".to_string(),
                    is_error: true,
                    images: Vec::new(),
                };
            }
        };
        let claim_token = match claim_token(&input) {
            Some(token) => token,
            None => {
                return ToolResult {
                    content: "Missing or invalid opaque 'claim_token'".to_string(),
                    is_error: true,
                    images: Vec::new(),
                };
            }
        };
        let note = input
            .get("completion_note")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match self
            .sink
            .complete(
                &self.owner_conversation_id,
                id,
                claim_generation,
                claim_token,
                note,
            )
            .await
        {
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
    owner_conversation_id: String,
}

impl RequirementUpdateStatusTool {
    pub fn new(
        sink: Arc<dyn RequirementSink>,
        owner_conversation_id: impl Into<String>,
    ) -> Self {
        Self {
            sink,
            owner_conversation_id: owner_conversation_id.into(),
        }
    }
}

#[async_trait]
impl Tool for RequirementUpdateStatusTool {
    fn name(&self) -> &str {
        "requirement_update_status"
    }

    fn description(&self) -> &str {
        "Update the status of the current AutoWork requirement. Use status='failed' with a reason \
         if you cannot complete it, or status='done' when finished. Pass the exact \
         claim_generation and claim_token from the current prompt. Valid: in_progress, done, failed."
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
                "claim_generation": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "The exact positive claim generation from the current AutoWork prompt"
                },
                "claim_token": {
                    "type": "string",
                    "minLength": 64,
                    "maxLength": 64,
                    "pattern": "^[0-9a-f]{64}$",
                    "description": "The opaque claim token from the current AutoWork prompt"
                },
                "status": {
                    "type": "string",
                    "enum": ["in_progress", "done", "failed"],
                    "description": "New status"
                },
                "note": { "type": "string", "description": "Optional reason / note" }
            },
            "required": ["id", "claim_generation", "claim_token", "status"]
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
        let claim_generation = match input.get("claim_generation").and_then(Value::as_i64) {
            Some(generation) if generation > 0 => generation,
            _ => {
                return ToolResult {
                    content: "Missing or invalid positive integer 'claim_generation'".to_string(),
                    is_error: true,
                    images: Vec::new(),
                };
            }
        };
        let claim_token = match claim_token(&input) {
            Some(token) => token,
            None => {
                return ToolResult {
                    content: "Missing or invalid opaque 'claim_token'".to_string(),
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
        match self
            .sink
            .update_status(
                &self.owner_conversation_id,
                id,
                claim_generation,
                claim_token,
                status,
                note,
            )
            .await
        {
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

    const OWNER_CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
    const CLAIM_TOKEN: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[derive(Default)]
    struct FakeSink {
        completed: Mutex<Vec<(String, String, i64, String, String)>>,
        statuses: Mutex<Vec<(String, String, i64, String, String, Option<String>)>>,
        fail: bool,
    }

    #[async_trait]
    impl RequirementSink for FakeSink {
        async fn complete(
            &self,
            owner_conversation_id: &str,
            id: &str,
            claim_generation: i64,
            claim_token: &str,
            note: &str,
        ) -> Result<(), String> {
            if self.fail {
                return Err("boom".into());
            }
            self.completed
                .lock()
                .unwrap()
                .push((
                    owner_conversation_id.to_string(),
                    id.to_string(),
                    claim_generation,
                    claim_token.to_string(),
                    note.to_string(),
                ));
            Ok(())
        }
        async fn update_status(
            &self,
            owner_conversation_id: &str,
            id: &str,
            claim_generation: i64,
            claim_token: &str,
            status: &str,
            note: Option<&str>,
        ) -> Result<(), String> {
            if self.fail {
                return Err("boom".into());
            }
            self.statuses.lock().unwrap().push((
                owner_conversation_id.to_string(),
                id.to_string(),
                claim_generation,
                claim_token.to_string(),
                status.to_string(),
                note.map(|s| s.to_string()),
            ));
            Ok(())
        }
    }

    #[test]
    fn native_tool_schemas_require_generation_and_opaque_token() {
        let sink: Arc<dyn RequirementSink> = Arc::new(FakeSink::default());
        let schemas = [
            RequirementCompleteTool::new(sink.clone(), OWNER_CONVERSATION_ID)
                .input_schema(),
            RequirementUpdateStatusTool::new(sink, OWNER_CONVERSATION_ID)
                .input_schema(),
        ];

        for schema in schemas {
            let required = schema
                .get("required")
                .and_then(Value::as_array)
                .expect("required fields");
            assert!(
                required
                    .iter()
                    .any(|field| field.as_str() == Some("claim_generation"))
            );
            assert!(
                required
                    .iter()
                    .any(|field| field.as_str() == Some("claim_token"))
            );
            assert_eq!(
                schema
                    .pointer("/properties/claim_token/pattern")
                    .and_then(Value::as_str),
                Some("^[0-9a-f]{64}$")
            );
        }
    }

    #[tokio::test]
    async fn complete_calls_sink() {
        let sink = Arc::new(FakeSink::default());
        let tool = RequirementCompleteTool::new(sink.clone(), OWNER_CONVERSATION_ID);
        let id = RequirementId::new().into_string();
        let res = tool
            .execute(json!({
                "id": id,
                "claim_generation": 7,
                "claim_token": CLAIM_TOKEN,
                "completion_note": "done it"
            }))
            .await;
        assert!(!res.is_error, "content: {}", res.content);
        let completed = sink.completed.lock().unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(
            completed[0],
            (
                OWNER_CONVERSATION_ID.to_string(),
                id,
                7,
                CLAIM_TOKEN.to_string(),
                "done it".to_string()
            )
        );
    }

    #[tokio::test]
    async fn complete_missing_id_is_error() {
        let sink = Arc::new(FakeSink::default());
        let tool = RequirementCompleteTool::new(sink, OWNER_CONVERSATION_ID);
        let res = tool
            .execute(json!({
                "claim_generation": 1,
                "claim_token": CLAIM_TOKEN,
                "completion_note": "x"
            }))
            .await;
        assert!(res.is_error);
    }

    #[tokio::test]
    async fn complete_rejects_non_uuid_string_id() {
        let sink = Arc::new(FakeSink::default());
        let tool = RequirementCompleteTool::new(sink, OWNER_CONVERSATION_ID);
        let res = tool
            .execute(json!({
                "id": "1",
                "claim_generation": 1,
                "claim_token": CLAIM_TOKEN,
                "completion_note": "done it"
            }))
            .await;
        assert!(res.is_error);
    }

    #[tokio::test]
    async fn complete_requires_positive_claim_generation() {
        let sink = Arc::new(FakeSink::default());
        let tool = RequirementCompleteTool::new(sink.clone(), OWNER_CONVERSATION_ID);
        let id = RequirementId::new().into_string();

        for input in [
            json!({
                "id": id,
                "claim_token": CLAIM_TOKEN,
                "completion_note": "missing"
            }),
            json!({
                "id": id,
                "claim_generation": 0,
                "claim_token": CLAIM_TOKEN,
                "completion_note": "zero"
            }),
            json!({
                "id": id,
                "claim_generation": "1",
                "claim_token": CLAIM_TOKEN,
                "completion_note": "string"
            }),
        ] {
            let result = tool.execute(input).await;
            assert!(result.is_error, "content: {}", result.content);
        }
        assert!(sink.completed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn complete_requires_canonical_opaque_claim_token() {
        let sink = Arc::new(FakeSink::default());
        let tool = RequirementCompleteTool::new(sink.clone(), OWNER_CONVERSATION_ID);
        let id = RequirementId::new().into_string();

        for token in [
            "",
            "0123456789abcdef",
            "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF",
            "g123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ] {
            let result = tool
                .execute(json!({
                    "id": id,
                    "claim_generation": 1,
                    "claim_token": token,
                    "completion_note": "invalid token"
                }))
                .await;
            assert!(result.is_error, "token={token:?}, content={}", result.content);
        }
        assert!(sink.completed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn update_status_validates_enum() {
        let sink = Arc::new(FakeSink::default());
        let tool = RequirementUpdateStatusTool::new(sink.clone(), OWNER_CONVERSATION_ID);
        let id = RequirementId::new().into_string();
        let bad = tool
            .execute(json!({
                "id": id,
                "claim_generation": 9,
                "claim_token": CLAIM_TOKEN,
                "status": "weird"
            }))
            .await;
        assert!(bad.is_error);
        let missing_generation = tool
            .execute(json!({
                "id": id,
                "claim_token": CLAIM_TOKEN,
                "status": "failed"
            }))
            .await;
        assert!(missing_generation.is_error);
        let good = tool
            .execute(json!({
                "id": id,
                "claim_generation": 9,
                "claim_token": CLAIM_TOKEN,
                "status": "failed",
                "note": "blocked"
            }))
            .await;
        assert!(!good.is_error, "content: {}", good.content);
        let statuses = sink.statuses.lock().unwrap();
        assert_eq!(
            statuses[0],
            (
                OWNER_CONVERSATION_ID.to_string(),
                id,
                9,
                CLAIM_TOKEN.to_string(),
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
        let tool = RequirementCompleteTool::new(sink, OWNER_CONVERSATION_ID);
        let id = RequirementId::new().into_string();
        let res = tool
            .execute(json!({
                "id": id,
                "claim_generation": 1,
                "claim_token": CLAIM_TOKEN,
                "completion_note": "x"
            }))
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("boom"));
    }
}

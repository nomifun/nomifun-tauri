use std::path::Path;

use nomifun_api_types::{ConversationArtifactResponse, ConversationResponse, MessageResponse, MessageSearchItem};
use nomifun_common::{
    AgentType, AppError, ConversationSource, ConversationStatus, MessagePosition, MessageStatus, MessageType,
    ProviderWithModel, now_ms,
};
use nomifun_db::MessageSearchRow;
use nomifun_db::models::{ConversationArtifactRow, ConversationRow, MessageRow};

pub(crate) const TOOL_CONTENT_COMPACT_THRESHOLD_BYTES: usize = 64 * 1024;
const TOOL_CONTENT_PREVIEW_CHARS: usize = 4096;
const WRITEBACK_RUNNING_STALE_MS: i64 = 5 * 60 * 1000;

/// Convert a database row into an API response DTO.
///
/// Parses string enum fields and JSON text fields back into typed values.
/// `data_dir` is required so the response can expose a derived
/// `is_temporary_workspace` flag without storing that attribute on disk —
/// see [`row_to_response_with_extra`].
pub fn row_to_response(row: ConversationRow, data_dir: &Path) -> Result<ConversationResponse, AppError> {
    let extra: serde_json::Value =
        serde_json::from_str(&row.extra).map_err(|e| AppError::Internal(format!("Invalid extra JSON: {e}")))?;
    row_to_response_with_extra(row, extra, data_dir)
}

/// Same as [`row_to_response`] but takes a pre-parsed `extra` value. Used
/// by callers that need to mutate `extra` (e.g. lazy `skills` backfill)
/// before building the response DTO.
///
/// Injects a derived `is_temporary_workspace: bool` into the returned
/// `extra` blob by checking whether `extra.workspace` sits under the
/// backend-managed `data_dir`. The flag is not persisted — it is
/// computed on every read so the frontend never has to pattern-match
/// the directory name. Old rows that have no such flag on disk
/// automatically gain it on read, which means no migration is needed.
pub fn row_to_response_with_extra(
    row: ConversationRow,
    mut extra: serde_json::Value,
    data_dir: &Path,
) -> Result<ConversationResponse, AppError> {
    let is_temporary_workspace = {
        let ws = extra.get("workspace").and_then(|v| v.as_str()).unwrap_or("");
        // Companion sessions own a fixed, permanent per-companion work folder.
        // It sits under the data dir but is NOT a throwaway temp workspace —
        // mark it non-temporary so the chat tab keeps the "open workspace folder"
        // affordance and doesn't mislabel a locked, browsable work path.
        let is_companion = extra.get("companionSession").and_then(|v| v.as_bool()).unwrap_or(false);
        !is_companion && !ws.is_empty() && Path::new(ws).starts_with(data_dir)
    };
    if let Some(obj) = extra.as_object_mut() {
        obj.insert(
            "is_temporary_workspace".to_owned(),
            serde_json::Value::Bool(is_temporary_workspace),
        );
    }

    let agent_type: AgentType = string_to_enum(&row.r#type)?;
    let status: ConversationStatus = match row.status.as_deref() {
        None | Some("") => ConversationStatus::Finished,
        Some(s) => string_to_enum(s)?,
    };

    let source: Option<ConversationSource> = row.source.as_deref().map(string_to_enum).transpose()?;

    let model: Option<ProviderWithModel> = row.model.as_deref().map(parse_provider_with_model).transpose()?;

    Ok(ConversationResponse {
        id: row.id,
        name: row.name,
        r#type: agent_type,
        model,
        status,
        runtime: None,
        source,
        pinned: row.pinned,
        pinned_at: row.pinned_at,
        channel_chat_id: row.channel_chat_id,
        created_at: row.created_at,
        modified_at: row.updated_at,
        extra,
    })
}

/// Parse the model JSON column into `ProviderWithModel`.
///
/// Nomi stores the full provider object (`TProviderWithModel`) which includes
/// fields like `id`, `platform`, `base_url`, `api_key`, `use_model`, and a `model`
/// field that can be an array of model objects. The backend only needs
/// `provider_id`, `model` (the selected model name), and `use_model`.
/// Accepts both snake_case and legacy camelCase key names for backward compatibility.
fn parse_provider_with_model(s: &str) -> Result<ProviderWithModel, AppError> {
    let v: serde_json::Value =
        serde_json::from_str(s).map_err(|e| AppError::Internal(format!("Invalid model JSON: {e}")))?;

    if let Some(provider_id) = v
        .get("provider_id")
        .or_else(|| v.get("providerId"))
        .and_then(|v| v.as_str())
    {
        let model = v.get("model").and_then(|v| v.as_str()).unwrap_or_default();
        let use_model = v
            .get("use_model")
            .or_else(|| v.get("useModel"))
            .and_then(|v| v.as_str())
            .map(String::from);
        return Ok(ProviderWithModel {
            provider_id: provider_id.to_string(),
            model: model.to_string(),
            use_model,
        });
    }

    if let Some(id) = v.get("id").and_then(|v| v.as_str()) {
        let use_model_str = v
            .get("use_model")
            .or_else(|| v.get("useModel"))
            .and_then(|v| v.as_str())
            .map(String::from);
        return Ok(ProviderWithModel {
            provider_id: id.to_string(),
            model: use_model_str.clone().unwrap_or_default(),
            use_model: use_model_str,
        });
    }

    Err(AppError::Internal(format!(
        "Model JSON missing both 'provider_id'/'providerId' and 'id': {s}"
    )))
}

/// Parse a DB string value into a typed enum via serde.
///
/// e.g. `"acp"` → `AgentType::Acp`
pub fn string_to_enum<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, AppError> {
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .map_err(|e| AppError::Internal(format!("Invalid enum value '{s}': {e}")))
}

/// Convert a message database row into an API response DTO.
pub fn row_to_message_response(row: MessageRow) -> Result<MessageResponse, AppError> {
    let msg_type: MessageType = string_to_enum(&row.r#type)?;

    let position: Option<MessagePosition> = row.position.as_deref().map(string_to_enum).transpose()?;

    let status: Option<MessageStatus> = row.status.as_deref().map(string_to_enum).transpose()?;

    let mut content: serde_json::Value = serde_json::from_str(&row.content)
        .map_err(|e| AppError::Internal(format!("Invalid message content JSON: {e}")))?;
    project_interrupted_writeback_state(&mut content);

    Ok(MessageResponse {
        id: row.id,
        conversation_id: row.conversation_id,
        msg_id: row.msg_id,
        r#type: msg_type,
        content,
        position,
        status,
        hidden: row.hidden,
        created_at: row.created_at,
    })
}

fn project_interrupted_writeback_state(content: &mut serde_json::Value) {
    let Some(writeback) = content
        .as_object_mut()
        .and_then(|obj| obj.get_mut("knowledge_writeback"))
    else {
        return;
    };
    let Some(status) = writeback.get("status").and_then(|v| v.as_str()) else {
        return;
    };
    if !matches!(status, "started" | "extracting" | "writing") {
        return;
    }
    let updated_at = writeback
        .get("updated_at")
        .and_then(|v| v.as_i64())
        .or_else(|| writeback.get("started_at").and_then(|v| v.as_i64()))
        .unwrap_or_default();
    if now_ms().saturating_sub(updated_at) < WRITEBACK_RUNNING_STALE_MS {
        return;
    }
    if let Some(obj) = writeback.as_object_mut() {
        obj.insert("status".to_owned(), serde_json::json!("interrupted"));
        obj.insert("retryable".to_owned(), serde_json::json!(true));
        obj.insert("interrupted_at".to_owned(), serde_json::json!(now_ms()));
    }
}

/// Convert a message row for history-list use, compacting oversized tool payloads.
pub fn row_to_message_response_compact(row: MessageRow) -> Result<MessageResponse, AppError> {
    let original_size = row.content.len();
    let mut response = row_to_message_response(row)?;
    if !is_tool_message(response.r#type) || original_size <= TOOL_CONTENT_COMPACT_THRESHOLD_BYTES {
        return Ok(response);
    }

    let mut truncated = false;
    truncate_large_strings(&mut response.content, TOOL_CONTENT_PREVIEW_CHARS, &mut truncated);
    if truncated && let Some(obj) = response.content.as_object_mut() {
        obj.insert(
            "_compact".to_string(),
            serde_json::json!({
                "truncated": true,
                "original_size": original_size,
                "preview_chars": TOOL_CONTENT_PREVIEW_CHARS
            }),
        );
    }

    Ok(response)
}

fn is_tool_message(msg_type: MessageType) -> bool {
    matches!(
        msg_type,
        MessageType::ToolCall | MessageType::ToolGroup | MessageType::AcpToolCall
    )
}

fn truncate_large_strings(value: &mut serde_json::Value, max_chars: usize, truncated: &mut bool) {
    match value {
        serde_json::Value::String(text) if text.chars().count() > max_chars => {
            let preview: String = text.chars().take(max_chars).collect();
            *text = format!("{preview}\n...[truncated]");
            *truncated = true;
        }
        serde_json::Value::Array(items) => {
            for item in items {
                truncate_large_strings(item, max_chars, truncated);
            }
        }
        serde_json::Value::Object(map) => {
            for entry in map.values_mut() {
                truncate_large_strings(entry, max_chars, truncated);
            }
        }
        _ => {}
    }
}

/// Convert an artifact database row into an API response DTO.
pub fn row_to_artifact_response(row: ConversationArtifactRow) -> Result<ConversationArtifactResponse, AppError> {
    let kind = string_to_enum(&row.kind)?;
    let status = string_to_enum(&row.status)?;
    let payload: serde_json::Value = serde_json::from_str(&row.payload)
        .map_err(|e| AppError::Internal(format!("Invalid artifact payload JSON: {e}")))?;

    Ok(ConversationArtifactResponse {
        id: row.id,
        conversation_id: row.conversation_id,
        cron_job_id: row.cron_job_id,
        kind,
        status,
        payload,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

/// Extract plain-text preview from a message content field.
///
/// Message content is stored as JSON (arrays, objects with nested strings).
/// This recursively collects all string values and joins them with spaces,
/// producing a flat preview suitable for search snippet display.
fn extract_preview_text(raw_content: &str) -> String {
    fn collect_strings(value: &serde_json::Value, bucket: &mut Vec<String>) {
        match value {
            serde_json::Value::String(s) => {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    bucket.push(trimmed.to_owned());
                }
            }
            serde_json::Value::Array(arr) => {
                for item in arr {
                    collect_strings(item, bucket);
                }
            }
            serde_json::Value::Object(map) => {
                for item in map.values() {
                    collect_strings(item, bucket);
                }
            }
            _ => {}
        }
    }

    match serde_json::from_str::<serde_json::Value>(raw_content) {
        Ok(parsed) => {
            let mut bucket = Vec::new();
            collect_strings(&parsed, &mut bucket);
            let joined = bucket.join(" ");
            let normalized = joined.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.is_empty() {
                raw_content.split_whitespace().collect::<Vec<_>>().join(" ")
            } else {
                normalized
            }
        }
        Err(_) => raw_content.split_whitespace().collect::<Vec<_>>().join(" "),
    }
}

/// Convert a search result row into an API search item DTO.
pub fn search_row_to_item(row: MessageSearchRow, data_dir: &Path) -> Result<MessageSearchItem, AppError> {
    let conversation_row = ConversationRow {
        id: row.conversation_id,
        user_id: String::new(),
        name: row.conversation_name,
        r#type: row.conversation_type,
        extra: row.conversation_extra,
        model: row.conversation_model,
        status: row.conversation_status,
        source: row.conversation_source,
        channel_chat_id: row.conversation_channel_chat_id,
        pinned: row.conversation_pinned,
        pinned_at: row.conversation_pinned_at,
        // Search rows don't project `cron_job_id`; it isn't needed for the
        // search-result conversation summary (no artifact card rendered there).
        cron_job_id: None,
        created_at: row.conversation_created_at,
        updated_at: row.conversation_updated_at,
    };

    let conversation = row_to_response(conversation_row, data_dir)?;
    let preview_text = extract_preview_text(&row.content);

    Ok(MessageSearchItem {
        message_id: row.message_id,
        message_type: row.r#type,
        message_created_at: row.created_at,
        preview_text,
        conversation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::{AgentType, ConversationSource, ConversationStatus};
    use serde_json::json;

    fn make_row(
        agent_type: &str,
        status: &str,
        source: Option<&str>,
        model_json: Option<&str>,
        extra_json: &str,
    ) -> ConversationRow {
        ConversationRow {
            id: 1,
            user_id: "user_1".into(),
            name: "Test".into(),
            r#type: agent_type.into(),
            extra: extra_json.into(),
            model: model_json.map(|s| s.into()),
            status: Some(status.into()),
            source: source.map(|s| s.into()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            cron_job_id: None,
            created_at: 1000,
            updated_at: 2000,
        }
    }

    fn make_message_row(content: serde_json::Value) -> MessageRow {
        MessageRow {
            id: "msg_1".into(),
            conversation_id: 1,
            msg_id: Some("msg_1".into()),
            r#type: "text".into(),
            content: content.to_string(),
            position: Some("left".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: 1000,
        }
    }

    #[test]
    fn row_to_response_basic() {
        let model = json!({"providerId": "p1", "model": "m1"});
        let row = make_row(
            "acp",
            "pending",
            Some("nomifun"),
            Some(&model.to_string()),
            r#"{"workspace": "/project"}"#,
        );
        let resp = row_to_response(row, Path::new("/tmp/data")).unwrap();
        assert_eq!(resp.id, 1);
        assert_eq!(resp.r#type, AgentType::Acp);
        assert_eq!(resp.status, ConversationStatus::Pending);
        assert_eq!(resp.source, Some(ConversationSource::Nomifun));
        assert_eq!(resp.model.unwrap().model, "m1");
        assert_eq!(resp.extra["workspace"], "/project");
        assert_eq!(resp.modified_at, 2000);
    }

    #[test]
    fn row_to_response_no_source() {
        let row = make_row("acp", "running", None, None, "{}");
        let resp = row_to_response(row, Path::new("/tmp/data")).unwrap();
        assert!(resp.source.is_none());
        assert!(resp.model.is_none());
    }

    #[test]
    fn row_to_response_invalid_type() {
        let row = make_row("invalid", "pending", None, None, "{}");
        let err = row_to_response(row, Path::new("/tmp/data")).unwrap_err();
        assert!(matches!(err, AppError::Internal(_)));
    }

    #[test]
    fn row_to_response_invalid_extra_json() {
        let row = ConversationRow {
            id: 1,
            user_id: "user_1".into(),
            name: "Test".into(),
            r#type: "acp".into(),
            extra: "not-json".into(),
            model: None,
            status: Some("pending".into()),
            source: None,
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            cron_job_id: None,
            created_at: 1000,
            updated_at: 2000,
        };
        let err = row_to_response(row, Path::new("/tmp/data")).unwrap_err();
        assert!(matches!(err, AppError::Internal(_)));
    }

    #[test]
    fn string_to_enum_valid() {
        let agent: AgentType = string_to_enum("acp").unwrap();
        assert_eq!(agent, AgentType::Acp);

        let status: ConversationStatus = string_to_enum("finished").unwrap();
        assert_eq!(status, ConversationStatus::Finished);

        let src: ConversationSource = string_to_enum("telegram").unwrap();
        assert_eq!(src, ConversationSource::Telegram);
    }

    #[test]
    fn string_to_enum_invalid() {
        let err = string_to_enum::<AgentType>("not_valid").unwrap_err();
        assert!(matches!(err, AppError::Internal(_)));
    }

    #[test]
    fn parse_provider_with_model_backend_format() {
        let json = r#"{"providerId":"p1","model":"claude-sonnet-4-20250514","useModel":"claude-sonnet"}"#;
        let result = parse_provider_with_model(json).unwrap();
        assert_eq!(result.provider_id, "p1");
        assert_eq!(result.model, "claude-sonnet-4-20250514");
        assert_eq!(result.use_model.as_deref(), Some("claude-sonnet"));
    }

    #[test]
    fn parse_provider_with_model_nomifun_format() {
        let json = r#"{"id":"prov_1","platform":"openai","name":"My Provider","baseUrl":"https://api.openai.com","apiKey":"sk-xxx","model":[{"id":"gpt-4","name":"GPT-4"}],"capabilities":["text","vision"],"useModel":"gpt-4-turbo","enabled":true}"#;
        let result = parse_provider_with_model(json).unwrap();
        assert_eq!(result.provider_id, "prov_1");
        assert_eq!(result.model, "gpt-4-turbo");
        assert_eq!(result.use_model.as_deref(), Some("gpt-4-turbo"));
    }

    #[test]
    fn parse_provider_with_model_missing_both_ids() {
        let json = r#"{"name":"invalid"}"#;
        assert!(parse_provider_with_model(json).is_err());
    }

    #[test]
    fn row_to_response_marks_workspace_inside_data_dir_as_temporary() {
        let row = make_row(
            "acp",
            "pending",
            Some("nomifun"),
            None,
            r#"{"workspace":"/srv/nomifun-data/conversations/claude-temp-abc"}"#,
        );
        let resp = row_to_response(row, Path::new("/srv/nomifun-data")).unwrap();
        assert_eq!(resp.extra["is_temporary_workspace"], true);
    }

    #[test]
    fn row_to_response_marks_workspace_outside_data_dir_as_non_temporary() {
        let row = make_row(
            "acp",
            "pending",
            Some("nomifun"),
            None,
            r#"{"workspace":"/Users/alice/my-project"}"#,
        );
        let resp = row_to_response(row, Path::new("/srv/nomifun-data")).unwrap();
        assert_eq!(resp.extra["is_temporary_workspace"], false);
    }

    #[test]
    fn row_to_response_marks_missing_workspace_as_non_temporary() {
        let row = make_row("acp", "pending", Some("nomifun"), None, r#"{}"#);
        let resp = row_to_response(row, Path::new("/srv/nomifun-data")).unwrap();
        assert_eq!(resp.extra["is_temporary_workspace"], false);
    }

    #[test]
    fn row_to_response_marks_companion_workspace_as_non_temporary() {
        // A companion's fixed work folder sits under the data dir but is a
        // permanent per-companion workspace, not a throwaway temp one — the
        // `companionSession` flag must override the under-data-dir heuristic.
        let row = make_row(
            "nomi",
            "pending",
            Some("nomifun"),
            None,
            r#"{"companionSession":true,"workspace":"/srv/nomifun-data/companion/companions/companion_x/workspace"}"#,
        );
        let resp = row_to_response(row, Path::new("/srv/nomifun-data")).unwrap();
        assert_eq!(resp.extra["is_temporary_workspace"], false);
    }

    #[test]
    fn row_with_pinned_at() {
        let row = ConversationRow {
            id: 2,
            user_id: "user_1".into(),
            name: "Pinned".into(),
            r#type: "acp".into(),
            extra: "{}".into(),
            model: None,
            status: Some("pending".into()),
            source: Some("nomifun".into()),
            channel_chat_id: Some("chat:1".into()),
            pinned: true,
            pinned_at: Some(5000),
            cron_job_id: None,
            created_at: 1000,
            updated_at: 3000,
        };
        let resp = row_to_response(row, Path::new("/tmp/data")).unwrap();
        assert!(resp.pinned);
        assert_eq!(resp.pinned_at, Some(5000));
        assert_eq!(resp.channel_chat_id.as_deref(), Some("chat:1"));
    }

    // ── extract_preview_text ───────────────────────────────────────────

    #[test]
    fn test_extract_preview_text_json_array() {
        let content = r#"[{"type":"text","content":"Hello world"},{"type":"text","content":"How are you?"}]"#;
        let result = extract_preview_text(content);
        assert!(result.contains("Hello world"));
        assert!(result.contains("How are you?"));
    }

    #[test]
    fn test_extract_preview_text_plain_string() {
        let content = "Just plain text message";
        let result = extract_preview_text(content);
        assert_eq!(result, "Just plain text message");
    }

    #[test]
    fn test_extract_preview_text_nested_object() {
        let content = r#"{"text":"nested value","items":[{"content":"inner"}]}"#;
        let result = extract_preview_text(content);
        assert!(result.contains("nested value"));
        assert!(result.contains("inner"));
    }

    #[test]
    fn test_extract_preview_text_malformed_json() {
        let content = "this is not { json at all";
        let result = extract_preview_text(content);
        assert_eq!(result, "this is not { json at all");
    }

    #[test]
    fn test_extract_preview_text_empty_content() {
        let result = extract_preview_text("");
        assert_eq!(result, "");
    }

    #[test]
    fn test_extract_preview_text_whitespace_normalization() {
        let content = r#"{"content":"  hello   world  "}"#;
        let result = extract_preview_text(content);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn row_to_message_response_projects_stale_writeback_as_interrupted() {
        let stale_at = now_ms() - WRITEBACK_RUNNING_STALE_MS - 1;
        let row = make_message_row(json!({
            "content": "answer",
            "knowledge_writeback": {
                "status": "writing",
                "attempt_id": "msg_1:1",
                "started_at": stale_at,
                "updated_at": stale_at,
                "retryable": false
            }
        }));

        let resp = row_to_message_response(row).unwrap();

        assert_eq!(resp.content["knowledge_writeback"]["status"], "interrupted");
        assert_eq!(resp.content["knowledge_writeback"]["retryable"], true);
        assert!(resp.content["knowledge_writeback"]["interrupted_at"].is_i64());
    }

    // ── search_row_to_item ─────────────────────────────────────────────

    #[test]
    fn test_search_row_to_item_builds_nested_conversation() {
        let row = MessageSearchRow {
            message_id: "msg_1".into(),
            r#type: "text".into(),
            content: r#"{"content":"hello world"}"#.into(),
            created_at: 5000,
            conversation_id: 1,
            conversation_name: "Test Conv".into(),
            conversation_type: "acp".into(),
            conversation_extra: r#"{"workspace":"/project"}"#.into(),
            conversation_model: None,
            conversation_status: Some("finished".into()),
            conversation_source: Some("nomifun".into()),
            conversation_channel_chat_id: None,
            conversation_pinned: false,
            conversation_pinned_at: None,
            conversation_created_at: 1000,
            conversation_updated_at: 2000,
        };

        let item = search_row_to_item(row, Path::new("/tmp/data")).unwrap();

        assert_eq!(item.message_id, "msg_1");
        assert_eq!(item.message_type, "text");
        assert_eq!(item.message_created_at, 5000);
        assert_eq!(item.preview_text, "hello world");

        assert_eq!(item.conversation.id, 1);
        assert_eq!(item.conversation.name, "Test Conv");
        assert_eq!(item.conversation.r#type, AgentType::Acp);
        assert_eq!(item.conversation.source, Some(ConversationSource::Nomifun));
        assert_eq!(item.conversation.extra["workspace"], "/project");
        assert_eq!(item.conversation.modified_at, 2000);
    }

    #[test]
    fn test_search_row_to_item_invalid_conversation_type() {
        let row = MessageSearchRow {
            message_id: "msg_1".into(),
            r#type: "text".into(),
            content: "plain text".into(),
            created_at: 5000,
            conversation_id: 1,
            conversation_name: "Test".into(),
            conversation_type: "invalid_type".into(),
            conversation_extra: "{}".into(),
            conversation_model: None,
            conversation_status: Some("finished".into()),
            conversation_source: None,
            conversation_channel_chat_id: None,
            conversation_pinned: false,
            conversation_pinned_at: None,
            conversation_created_at: 1000,
            conversation_updated_at: 2000,
        };

        let err = search_row_to_item(row, Path::new("/tmp/data")).unwrap_err();
        assert!(matches!(err, AppError::Internal(_)));
    }

    #[test]
    fn test_search_row_to_item_invalid_conversation_extra_json() {
        let row = MessageSearchRow {
            message_id: "msg_1".into(),
            r#type: "text".into(),
            content: r#"{"content":"hello"}"#.into(),
            created_at: 5000,
            conversation_id: 1,
            conversation_name: "Test".into(),
            conversation_type: "acp".into(),
            conversation_extra: "not valid json".into(),
            conversation_model: None,
            conversation_status: Some("finished".into()),
            conversation_source: None,
            conversation_channel_chat_id: None,
            conversation_pinned: false,
            conversation_pinned_at: None,
            conversation_created_at: 1000,
            conversation_updated_at: 2000,
        };

        let err = search_row_to_item(row, Path::new("/tmp/data")).unwrap_err();
        assert!(matches!(err, AppError::Internal(_)));
    }
}

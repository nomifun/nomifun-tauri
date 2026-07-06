use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use nomi_types::llm::{LlmEvent, LlmRequest};
use nomi_types::message::{ContentBlock, Message, Role, StopReason, TokenUsage};
use nomi_types::tool::{ToolDef, truncate_deferred_description};

use crate::anthropic_shared::StreamOutcome;
use crate::{LlmProvider, ProviderError};
use nomi_config::compat::ProviderCompat;

pub struct OpenAIProvider {
    api_key: String,
    base_url: String,
    compat: ProviderCompat,
}

impl OpenAIProvider {
    pub fn new(api_key: &str, base_url: &str, compat: ProviderCompat) -> Self {
        Self {
            api_key: api_key.to_string(),
            base_url: base_url.to_string(),
            compat,
        }
    }

    fn build_headers(&self) -> Result<HeaderMap, ProviderError> {
        let mut headers = HeaderMap::new();
        let bearer = format!("Bearer {}", self.api_key);
        let auth = HeaderValue::from_str(&bearer).map_err(|e| {
            ProviderError::Connection(format!("Invalid authorization header: {}", e))
        })?;
        headers.insert(AUTHORIZATION, auth);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(headers)
    }

    fn build_messages(messages: &[Message], system: &str, compat: &ProviderCompat) -> Vec<Value> {
        let mut result: Vec<Value> = Vec::new();

        // Check if any assistant message in the conversation has thinking content.
        // If so, DeepSeek API requires ALL assistant messages to include
        // reasoning_content (even if empty string).
        let has_any_thinking = messages.iter().any(|m| {
            m.role == Role::Assistant
                && m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Thinking { .. }))
        });

        // System message first
        if !system.is_empty() {
            result.push(json!({
                "role": "system",
                "content": system
            }));
        }

        for msg in messages {
            match msg.role {
                Role::User => {
                    // Check if this contains tool results
                    let has_tool_results = msg
                        .content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolResult { .. }));

                    if has_tool_results {
                        // Each tool result becomes a separate "tool" role message.
                        // The OpenAI wire format has no is_error flag, so failed
                        // results are prefixed textually — otherwise the model
                        // can't tell a tool error from successful output.
                        for block in &msg.content {
                            if let ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                is_error,
                                images,
                            } = block
                            {
                                let content = if *is_error {
                                    format!("[tool error] {content}")
                                } else {
                                    content.clone()
                                };
                                result.push(json!({
                                    "role": "tool",
                                    "tool_call_id": tool_use_id,
                                    "content": content
                                }));
                                if let Some(img_msg) = tool_images_user_message(
                                    tool_use_id,
                                    images,
                                    compat.supports_image(),
                                ) {
                                    result.push(img_msg);
                                }
                            }
                        }
                    } else {
                        // Check if the message contains any image blocks
                        let has_images = msg
                            .content
                            .iter()
                            .any(|b| matches!(b, ContentBlock::Image { .. }));

                        if has_images {
                            // Multimodal user message: build content array with
                            // text and image_url parts.
                            let mut parts: Vec<Value> = Vec::new();
                            let mut stripped_images = 0usize;
                            for block in &msg.content {
                                match block {
                                    ContentBlock::Text { text } => {
                                        let text = strip_patterns_from_text(text, compat);
                                        if !text.is_empty() {
                                            parts.push(json!({
                                                "type": "text",
                                                "text": text
                                            }));
                                        }
                                    }
                                    ContentBlock::Image { media_type, data } => {
                                        if compat.supports_image() {
                                            parts.push(json!({
                                                "type": "image_url",
                                                "image_url": {
                                                    "url": format!("data:{media_type};base64,{data}")
                                                }
                                            }));
                                        } else {
                                            stripped_images += 1;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            if stripped_images > 0 {
                                parts.push(json!({
                                    "type": "text",
                                    "text": "[图片已省略：当前模型不支持图片输入]"
                                }));
                            }
                            result.push(json!({
                                "role": "user",
                                "content": parts
                            }));
                        } else {
                            let text: String = msg
                                .content
                                .iter()
                                .filter_map(|b| {
                                    if let ContentBlock::Text { text } = b {
                                        Some(text.as_str())
                                    } else {
                                        None
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            let text = strip_patterns_from_text(&text, compat);
                            result.push(json!({
                                "role": "user",
                                "content": text
                            }));
                        }
                    }
                }
                Role::Assistant => {
                    let mut msg_json = json!({ "role": "assistant" });

                    // Preserve reasoning_content for models with thinking mode
                    // (e.g. DeepSeek Reasoner, Kimi K2.5). The API requires
                    // ALL assistant messages to include reasoning_content once
                    // any message in the conversation has it.
                    let thinking: String = msg
                        .content
                        .iter()
                        .filter_map(|b| {
                            if let ContentBlock::Thinking { thinking, .. } = b {
                                Some(thinking.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("");

                    if has_any_thinking {
                        msg_json["reasoning_content"] = json!(thinking);
                    }

                    let text: String = msg
                        .content
                        .iter()
                        .filter_map(|b| {
                            if let ContentBlock::Text { text } = b {
                                Some(text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    let text = strip_patterns_from_text(&text, compat);

                    let tool_calls: Vec<Value> = msg
                        .content
                        .iter()
                        .filter_map(|b| {
                            if let ContentBlock::ToolUse {
                                id,
                                name,
                                input,
                                extra,
                            } = b
                            {
                                let mut tc_json = json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": serde_json::to_string(input).unwrap_or_default()
                                    }
                                });
                                if let Some(extra_val) = extra {
                                    tc_json["extra_content"] = extra_val.clone();
                                }
                                Some(tc_json)
                            } else {
                                None
                            }
                        })
                        .collect();

                    if !text.is_empty() {
                        msg_json["content"] = json!(text);
                    } else if tool_calls.is_empty() {
                        msg_json["content"] = json!("");
                    }

                    if !tool_calls.is_empty() {
                        msg_json["tool_calls"] = json!(tool_calls);
                    }

                    result.push(msg_json);
                }
                Role::System => {
                    // Already handled above
                }
                Role::Tool => {
                    for block in &msg.content {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                            images,
                        } = block
                        {
                            let content = if *is_error {
                                format!("[tool error] {content}")
                            } else {
                                content.clone()
                            };
                            result.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content
                            }));
                            if let Some(img_msg) = tool_images_user_message(
                                tool_use_id,
                                images,
                                compat.supports_image(),
                            ) {
                                result.push(img_msg);
                            }
                        }
                    }
                }
            }
        }

        // Dedup tool results: keep last occurrence of each tool_call_id
        if compat.dedup_tool_results() {
            dedup_tool_results(&mut result);
        }

        // Clean orphan tool calls: remove tool_call entries with no matching tool result
        if compat.clean_orphan_tool_calls() {
            clean_orphaned_tool_calls(&mut result);
        }

        // Merge consecutive assistant messages
        if compat.merge_assistant_messages() {
            merge_consecutive_assistant(&mut result);
        }

        result
    }

    fn build_tools(tools: &[ToolDef]) -> Vec<Value> {
        tools
            .iter()
            .map(|t| {
                if t.deferred {
                    let short_desc = truncate_deferred_description(&t.description);
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": format!(
                                "(Deferred) {short_desc} — Use ToolSearch to load full schema before calling."
                            ),
                            "parameters": {
                                "type": "object",
                                "properties": {}
                            }
                        }
                    })
                } else {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema
                        }
                    })
                }
            })
            .collect()
    }

    fn build_request_body(&self, request: &LlmRequest) -> Value {
        let max_tokens_field = self
            .compat
            .max_tokens_field
            .as_deref()
            .unwrap_or("max_tokens");

        let mut body = json!({
            "model": request.model,
            "messages": Self::build_messages(&request.messages, &request.system, &self.compat),
            "stream": true,
            "stream_options": { "include_usage": true }
        });
        body[max_tokens_field] = json!(request.max_tokens);

        if !request.tools.is_empty() {
            body["tools"] = json!(Self::build_tools(&request.tools));
        }

        if let Some(effort) = &request.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }

        body
    }
}

/// Generate a unique tool call ID in OpenAI `call_xxx` format. UUIDv7
/// (time-ordered + random) is collision-free even within the same instant.
fn generate_call_id() -> String {
    format!("call_{}", uuid::Uuid::now_v7().simple())
}

/// Build a follow-up user message carrying a tool result's images.
///
/// The OpenAI wire format only allows string content in `tool` role
/// messages, so images ride in a separate user message right after the
/// tool result, labelled with the originating call id.
fn tool_images_user_message(
    tool_use_id: &str,
    images: &[nomi_types::tool::ToolImage],
    supports_image: bool,
) -> Option<Value> {
    if images.is_empty() || !supports_image {
        return None;
    }
    let mut parts: Vec<Value> = vec![json!({
        "type": "text",
        "text": format!("[images from tool call {tool_use_id}]")
    })];
    parts.extend(images.iter().map(|img| {
        json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{};base64,{}", img.media_type, img.data) }
        })
    }));
    Some(json!({ "role": "user", "content": parts }))
}

/// Strip configured patterns from text content
fn strip_patterns_from_text(text: &str, compat: &ProviderCompat) -> String {
    match &compat.strip_patterns {
        Some(patterns) if !patterns.is_empty() => {
            let mut result = text.to_string();
            for pattern in patterns {
                result = result.replace(pattern, "");
            }
            result
        }
        _ => text.to_string(),
    }
}

/// Deduplicate tool results: keep last occurrence of each tool_call_id
fn dedup_tool_results(messages: &mut Vec<Value>) {
    use std::collections::HashMap;

    // Find the last index of each tool_call_id
    let mut last_index: HashMap<String, usize> = HashMap::new();
    for (i, msg) in messages.iter().enumerate() {
        if msg["role"].as_str() == Some("tool")
            && let Some(id) = msg["tool_call_id"].as_str()
        {
            last_index.insert(id.to_string(), i);
        }
    }

    // Keep only the last occurrence
    let mut seen: HashMap<String, bool> = HashMap::new();
    let mut to_remove = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        if msg["role"].as_str() == Some("tool")
            && let Some(id) = msg["tool_call_id"].as_str()
            && let Some(&last_i) = last_index.get(id)
        {
            if i != last_i && !seen.contains_key(id) {
                to_remove.push(i);
            }
            if i == last_i {
                seen.insert(id.to_string(), true);
            }
        }
    }

    // Remove in reverse order to preserve indices
    for i in to_remove.into_iter().rev() {
        messages.remove(i);
    }
}

/// Remove tool_call entries from assistant messages that have no corresponding tool result
fn clean_orphaned_tool_calls(messages: &mut [Value]) {
    use std::collections::HashSet;

    let answered_ids: HashSet<String> = messages
        .iter()
        .filter(|m| m["role"].as_str() == Some("tool"))
        .filter_map(|m| m["tool_call_id"].as_str().map(String::from))
        .collect();

    for msg in messages.iter_mut() {
        if msg["role"].as_str() == Some("assistant")
            && let Some(tcs) = msg["tool_calls"].as_array_mut()
        {
            tcs.retain(|tc| {
                tc["id"]
                    .as_str()
                    .map(|id| answered_ids.contains(id))
                    .unwrap_or(true)
            });
            if tcs.is_empty() {
                msg.as_object_mut().unwrap().remove("tool_calls");
            }
        }
    }
}

/// Merge consecutive assistant messages into one
fn merge_consecutive_assistant(messages: &mut Vec<Value>) {
    let mut i = 0;
    while i + 1 < messages.len() {
        if messages[i]["role"].as_str() == Some("assistant")
            && messages[i + 1]["role"].as_str() == Some("assistant")
        {
            let next = messages.remove(i + 1);

            // Merge text content
            let curr_text = messages[i]["content"].as_str().unwrap_or("").to_string();
            let next_text = next["content"].as_str().unwrap_or("").to_string();
            let merged_text = match (curr_text.is_empty(), next_text.is_empty()) {
                (true, true) => String::new(),
                (true, false) => next_text,
                (false, true) => curr_text,
                (false, false) => format!("{}{}", curr_text, next_text),
            };

            if !merged_text.is_empty() {
                messages[i]["content"] = json!(merged_text);
            }

            // Merge reasoning_content
            let curr_rc = messages[i]["reasoning_content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let next_rc = next["reasoning_content"].as_str().unwrap_or("").to_string();
            let merged_rc = match (curr_rc.is_empty(), next_rc.is_empty()) {
                (true, true) => String::new(),
                (true, false) => next_rc,
                (false, true) => curr_rc,
                (false, false) => format!("{}{}", curr_rc, next_rc),
            };

            if !merged_rc.is_empty() {
                messages[i]["reasoning_content"] = json!(merged_rc);
            }

            // Merge tool_calls
            if let Some(next_tcs) = next["tool_calls"].as_array() {
                let curr_tcs = messages[i]
                    .as_object_mut()
                    .unwrap()
                    .entry("tool_calls")
                    .or_insert_with(|| json!([]));
                if let Some(arr) = curr_tcs.as_array_mut() {
                    arr.extend(next_tcs.iter().cloned());
                }
            }

            // Don't increment i - check the merged result against the next message
        } else {
            i += 1;
        }
    }
}

/// State for accumulating tool call deltas by index
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
    extra: Option<Value>,
    announced: bool,
    last_progress_signature: String,
}

struct TextToolCallAccumulator {
    id: String,
    announced: bool,
    last_progress_signature: String,
}

struct StreamState {
    tool_calls: Vec<ToolCallAccumulator>,
    text_tool_calls: Vec<TextToolCallAccumulator>,
    input_tokens: u64,
    output_tokens: u64,
    /// Deferred Done event: populated when finish_reason arrives, emitted on
    /// [DONE] so the final usage-only chunk has a chance to update token counts.
    pending_done: Option<LlmEvent>,
    /// Accumulated `content` / `reasoning_content` text across the stream. Used
    /// ONLY as a fallback at finish: some models (e.g. Qwen/Hermes-style, and
    /// step reasoning models under load) intermittently emit a tool call as a
    /// `<tool_call>…</tool_call>` block in the TEXT/REASONING channel instead of
    /// the structured `tool_calls` field. Without recovery the turn dead-ends with
    /// no action (looks "stuck"). When finish arrives with NO structured tool calls
    /// we scan these buffers and parse any embedded call. The happy path (structured
    /// tool_calls present) never touches this.
    content_buf: String,
    reasoning_buf: String,
    visible_content_buf: String,
    visible_reasoning_buf: String,
}

impl StreamState {
    fn new() -> Self {
        Self {
            tool_calls: Vec::new(),
            text_tool_calls: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            pending_done: None,
            content_buf: String::new(),
            reasoning_buf: String::new(),
            visible_content_buf: String::new(),
            visible_reasoning_buf: String::new(),
        }
    }

    /// Emit the deferred Done event with up-to-date token counts.
    ///
    /// OpenAI sends usage in a separate trailing chunk (choices:[]) *after* the
    /// chunk that carries `finish_reason`. We defer the Done event until [DONE]
    /// so that token counts are always accurate.
    fn flush_done(&mut self) -> Option<LlmEvent> {
        let pending = self.pending_done.take()?;
        Some(match pending {
            LlmEvent::Done { stop_reason, .. } => LlmEvent::Done {
                stop_reason,
                usage: TokenUsage {
                    input_tokens: self.input_tokens,
                    output_tokens: self.output_tokens,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                },
            },
            other => other,
        })
    }

    fn get_or_create_tool(&mut self, index: usize) -> &mut ToolCallAccumulator {
        while self.tool_calls.len() <= index {
            self.tool_calls.push(ToolCallAccumulator {
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
                extra: None,
                announced: false,
                last_progress_signature: String::new(),
            });
        }
        &mut self.tool_calls[index]
    }

    fn get_or_create_text_tool(&mut self, index: usize) -> &mut TextToolCallAccumulator {
        while self.text_tool_calls.len() <= index {
            self.text_tool_calls.push(TextToolCallAccumulator {
                id: String::new(),
                announced: false,
                last_progress_signature: String::new(),
            });
        }
        &mut self.text_tool_calls[index]
    }
}

#[async_trait]
impl LlmProvider for OpenAIProvider {
    async fn stream(
        &self,
        request: &LlmRequest,
    ) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        let url = format!("{}{}", self.base_url, self.compat.api_path());
        let body = self.build_request_body(request);
        let headers = self.build_headers()?;
        let client = crate::http_client();

        tracing::debug!(target: "nomi_providers", body = %serde_json::to_string_pretty(&body).unwrap_or_default(), "outgoing request");

        let response = crate::retry::with_initial_connect_retry(|| async {
            let response = client
                .post(&url)
                .headers(headers.clone())
                .json(&body)
                .send()
                .await?;

            let status = response.status();
            if !status.is_success() {
                let retry_after_ms = crate::parse_retry_after_ms(response.headers()).unwrap_or(5000);
                let body_text = response.text().await.unwrap_or_default();
                if status.as_u16() == 429 {
                    return Err(ProviderError::RateLimited {
                        retry_after_ms,
                        message: crate::non_empty_rate_limit_message(body_text),
                    });
                }
                return Err(ProviderError::Api {
                    status: status.as_u16(),
                    message: body_text,
                });
            }

            Ok(response)
        })
        .await?;

        let (tx, rx) = mpsc::channel(64);
        let auto_tool_id = self.compat.auto_tool_id();
        let client = client.clone();
        let url_clone = url.clone();

        tokio::spawn(async move {
            match process_sse_stream(response, &tx, auto_tool_id).await {
                StreamOutcome::Ok => {}
                StreamOutcome::FailedPartial(e) => {
                    let _ = tx.send(LlmEvent::Error(e.to_string())).await;
                }
                StreamOutcome::FailedEmpty(e) => {
                    if e.is_retryable() {
                        let mut backoff = std::time::Duration::from_secs(1);
                        let mut final_err = Some(e);
                        for attempt in 1..=crate::retry::MAX_STREAM_RETRIES {
                            backoff = crate::retry::backoff_sleep(attempt, backoff).await;
                            match crate::retry::send_and_check(&client, &url_clone, &headers, &body)
                                .await
                            {
                                Ok(resp) => {
                                    let outcome = process_sse_stream(resp, &tx, auto_tool_id).await;
                                    match crate::retry::evaluate_outcome(outcome, attempt) {
                                        Ok(None) => {
                                            final_err = None;
                                            break;
                                        }
                                        Ok(Some(e)) => {
                                            final_err = Some(e);
                                            break;
                                        }
                                        Err(_) => continue,
                                    }
                                }
                                Err(e) if attempt == crate::retry::MAX_STREAM_RETRIES => {
                                    final_err = Some(e);
                                    break;
                                }
                                Err(_) => continue,
                            }
                        }
                        if let Some(err) = final_err {
                            let _ = tx.send(LlmEvent::Error(err.to_string())).await;
                        }
                    } else {
                        let _ = tx.send(LlmEvent::Error(e.to_string())).await;
                    }
                }
            }
        });

        Ok(rx)
    }
}

async fn process_sse_stream(
    response: reqwest::Response,
    tx: &mpsc::Sender<LlmEvent>,
    auto_tool_id: bool,
) -> StreamOutcome {
    use futures::StreamExt;

    let mut state = StreamState::new();
    let mut buffer = String::new();
    let mut stream = response.bytes_stream();
    let mut emitted_content = false;

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let err = ProviderError::Connection(e.to_string());
                return if emitted_content {
                    StreamOutcome::FailedPartial(err)
                } else {
                    StreamOutcome::FailedEmpty(err)
                };
            }
        };
        let text = String::from_utf8_lossy(&chunk);
        buffer.push_str(&text);

        // Process complete lines
        while let Some(line_end) = buffer.find('\n') {
            let line = buffer[..line_end].trim().to_string();
            buffer = buffer[line_end + 1..].to_string();

            if line.is_empty() || line.starts_with(':') {
                continue;
            }

            if let Some(data) = line.strip_prefix("data: ") {
                tracing::debug!(target: "nomi_providers", chunk = %data, "sse chunk received");
                if data == "[DONE]" {
                    // Flush the deferred Done event now that the final
                    // usage-only chunk (choices:[]) has updated token counts.
                    if let Some(done) = state.flush_done() {
                        let _ = tx.send(done).await;
                    }
                    return StreamOutcome::Ok;
                }

                let events = parse_sse_chunk(data, &mut state, auto_tool_id);
                for event in events {
                    if matches!(
                        event,
                        LlmEvent::TextDelta(_)
                            | LlmEvent::ThinkingDelta(_)
                            | LlmEvent::ToolUseDelta { .. }
                            | LlmEvent::ToolUse { .. }
                    ) {
                        emitted_content = true;
                    }
                    if tx.send(event).await.is_err() {
                        return StreamOutcome::Ok;
                    }
                }
            }
        }
    }

    // The stream ended without an explicit `[DONE]` sentinel — some
    // OpenAI-compatible servers (vLLM, local deployments) just close the
    // connection after the final chunk. Flush any deferred Done so the consumer
    // always receives a terminal event instead of hanging. (Phase 1)
    if let Some(done) = state.flush_done() {
        let _ = tx.send(done).await;
    }
    StreamOutcome::Ok
}

/// Fallback tool-call recovery: some models (Qwen/Hermes-style, and step
/// reasoning models intermittently under load) emit a tool call as a
/// `<tool_call>…</tool_call>` block in the TEXT/REASONING channel instead of the
/// structured `tool_calls` field. This is only consulted when the structured
/// accumulator is empty at finish, so it never affects the normal path. Reasoning
/// is scanned first (step puts the call there), then content. Returns `None` when
/// no parseable call is found (turn dead-ends as before — no regression).
fn recover_text_tool_calls(state: &mut StreamState) -> Option<Vec<LlmEvent>> {
    let mut calls = parse_text_tool_calls(&state.reasoning_buf);
    if calls.is_empty() {
        calls = parse_text_tool_calls(&state.content_buf);
    }
    if calls.is_empty() {
        return None;
    }
    Some(
        calls
            .into_iter()
            .enumerate()
            .map(|(index, (name, input))| {
                let progress = state.get_or_create_text_tool(index);
                if progress.id.is_empty() {
                    progress.id = generate_call_id();
                }
                LlmEvent::ToolUse {
                    id: progress.id.clone(),
                    name,
                    input,
                    extra: None,
                }
            })
            .collect(),
    )
}

fn visible_text_without_tool_call_blocks(text: &str) -> String {
    let mut visible = String::new();
    let mut rest = text;

    loop {
        let Some(start) = rest.find("<tool_call>") else {
            visible.push_str(rest);
            break;
        };
        visible.push_str(&rest[..start]);
        let after = &rest[start + "<tool_call>".len()..];
        let Some(end) = after.find("</tool_call>") else {
            break;
        };
        rest = &after[end + "</tool_call>".len()..];
    }

    visible
}

fn append_raw_text_and_visible_delta(
    raw_buffer: &mut String,
    visible_buffer: &mut String,
    delta: &str,
) -> String {
    raw_buffer.push_str(delta);
    let next_visible = visible_text_without_tool_call_blocks(raw_buffer);
    let visible_delta = next_visible
        .strip_prefix(visible_buffer.as_str())
        .unwrap_or_default()
        .to_string();
    *visible_buffer = next_visible;
    visible_delta
}

struct TextToolCallPreview {
    name: String,
    input: Option<Value>,
}

/// Extract every `<tool_call>…</tool_call>` block from `text` and parse each into
/// (name, arguments). Handles both the Hermes JSON form
/// (`<tool_call>{"name":..,"arguments":{..}}</tool_call>`) and the Qwen XML form
/// (`<tool_call><function=NAME><parameter=KEY>VALUE</parameter>…</function></tool_call>`).
fn parse_text_tool_calls(text: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<tool_call>") {
        let after = &rest[start + "<tool_call>".len()..];
        let (block, next) = match after.find("</tool_call>") {
            Some(end) => (&after[..end], &after[end + "</tool_call>".len()..]),
            None => (after, ""),
        };
        if let Some(call) = parse_one_tool_call(block.trim()) {
            out.push(call);
        }
        rest = next;
    }
    out
}

fn parse_text_tool_call_progress(text: &str) -> Vec<TextToolCallPreview> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<tool_call>") {
        let after = &rest[start + "<tool_call>".len()..];
        let (block, next) = match after.find("</tool_call>") {
            Some(end) => (&after[..end], &after[end + "</tool_call>".len()..]),
            None => (after, ""),
        };
        if let Some(call) = parse_one_tool_call_progress(block.trim()) {
            out.push(call);
        }
        if next.is_empty() {
            break;
        }
        rest = next;
    }
    out
}

fn parse_one_tool_call_progress(block: &str) -> Option<TextToolCallPreview> {
    if block.starts_with('{') {
        if let Some((name, input)) = parse_one_tool_call(block) {
            return Some(TextToolCallPreview {
                name,
                input: tool_argument_value_progress_preview(&input),
            });
        }

        let name = extract_json_string_field(block, "name")?.trim().to_string();
        if name.is_empty() {
            return None;
        }

        let input = tool_argument_progress_preview(block).or_else(|| {
            let unescaped = block.replace("\\\"", "\"");
            if unescaped == block {
                None
            } else {
                tool_argument_progress_preview(&unescaped)
            }
        });
        return Some(TextToolCallPreview { name, input });
    }

    let name = str_between(block, "<function=", ">")?.trim().to_string();
    if name.is_empty() {
        return None;
    }

    Some(TextToolCallPreview {
        name,
        input: xml_parameter_progress_preview(block),
    })
}

fn parse_one_tool_call(block: &str) -> Option<(String, Value)> {
    // Hermes JSON form.
    if block.starts_with('{') {
        let v: Value = serde_json::from_str(block).ok()?;
        let name = v.get("name")?.as_str()?.trim().to_string();
        if name.is_empty() {
            return None;
        }
        let args = match v.get("arguments").cloned() {
            Some(Value::String(s)) => {
                serde_json::from_str(&s).unwrap_or(Value::Object(serde_json::Map::new()))
            }
            Some(other) => other,
            None => Value::Object(serde_json::Map::new()),
        };
        return Some((name, args));
    }
    // Qwen XML form.
    let name = str_between(block, "<function=", ">")?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let mut args = serde_json::Map::new();
    let mut rest = block;
    while let Some(ps) = rest.find("<parameter=") {
        let after = &rest[ps + "<parameter=".len()..];
        let Some(key_end) = after.find('>') else { break };
        let key = after[..key_end].trim().to_string();
        let val_start = &after[key_end + 1..];
        let (raw_val, next) = match val_start.find("</parameter>") {
            Some(e) => (&val_start[..e], &val_start[e + "</parameter>".len()..]),
            None => (val_start, ""),
        };
        let val_trim = raw_val.trim();
        // Parse the value as JSON when it is (arrays/objects/numbers/bools); else
        // keep the raw string.
        let val = serde_json::from_str::<Value>(val_trim)
            .unwrap_or_else(|_| Value::String(val_trim.to_string()));
        if !key.is_empty() {
            args.insert(key, val);
        }
        rest = next;
    }
    Some((name, Value::Object(args)))
}

/// The substring of `s` strictly between the first `start` and the next `end`
/// after it, or `None` if either delimiter is absent.
fn str_between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = s.find(start)? + start.len();
    let j = s[i..].find(end)? + i;
    Some(&s[i..j])
}

const TOOL_PROGRESS_PREVIEW_FIELDS: &[&str] = &[
    "file_path",
    "filePath",
    "path",
    "file_name",
    "fileName",
    "relative_path",
    "relativePath",
    "dir",
    "glob",
    "command",
    "cmd",
    "script",
    "pattern",
    "query",
    "url",
    "skill",
];

const RECOVERED_PARTIAL_WRITE_KEY: &str = "__nomi_recovered_partial_write";

fn tool_argument_value_progress_preview(input: &Value) -> Option<Value> {
    let Value::Object(map) = input else {
        return None;
    };

    let mut preview = serde_json::Map::new();
    for key in TOOL_PROGRESS_PREVIEW_FIELDS {
        if let Some(value) = map.get(*key)
            && is_small_preview_value(value)
        {
            preview.insert((*key).to_string(), value.clone());
        }
    }

    if preview.is_empty() {
        None
    } else {
        Some(Value::Object(preview))
    }
}

fn tool_argument_progress_preview(arguments: &str) -> Option<Value> {
    let mut preview = serde_json::Map::new();

    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(arguments) {
        return tool_argument_value_progress_preview(&Value::Object(map));
    } else {
        for key in TOOL_PROGRESS_PREVIEW_FIELDS {
            if let Some(value) = extract_json_string_field(arguments, key) {
                preview.insert((*key).to_string(), Value::String(value));
            }
        }
    }

    if preview.is_empty() {
        None
    } else {
        Some(Value::Object(preview))
    }
}

fn xml_parameter_progress_preview(block: &str) -> Option<Value> {
    let mut preview = serde_json::Map::new();

    for key in TOOL_PROGRESS_PREVIEW_FIELDS {
        if let Some(value) = extract_xml_parameter_text(block, key)
            && value.len() <= 2_000
        {
            preview.insert((*key).to_string(), Value::String(value));
        }
    }

    if preview.is_empty() {
        None
    } else {
        Some(Value::Object(preview))
    }
}

fn extract_xml_parameter_text(block: &str, key: &str) -> Option<String> {
    let start_tag = format!("<parameter={key}>");
    let start = block.find(&start_tag)? + start_tag.len();
    let raw = match block[start..].find("</parameter>") {
        Some(end) => &block[start..start + end],
        None => &block[start..],
    };
    let value = raw.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn is_small_preview_value(value: &Value) -> bool {
    match value {
        Value::String(s) => s.len() <= 2_000,
        Value::Number(_) | Value::Bool(_) => true,
        _ => false,
    }
}

fn extract_json_string_field(arguments: &str, key: &str) -> Option<String> {
    let quoted_key = format!("\"{key}\"");
    let mut search_from = 0usize;

    while let Some(relative_pos) = arguments[search_from..].find(&quoted_key) {
        let mut cursor = search_from + relative_pos + quoted_key.len();
        cursor = skip_json_whitespace(arguments, cursor);
        if arguments[cursor..].chars().next()? != ':' {
            search_from = cursor;
            continue;
        }
        cursor += ':'.len_utf8();
        cursor = skip_json_whitespace(arguments, cursor);
        if arguments[cursor..].chars().next()? != '"' {
            search_from = cursor;
            continue;
        }
        cursor += '"'.len_utf8();

        let mut escaped = false;
        for (offset, ch) in arguments[cursor..].char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                let end = cursor + offset;
                let raw = &arguments[cursor..end];
                let quoted = format!("\"{raw}\"");
                return serde_json::from_str::<String>(&quoted)
                    .ok()
                    .or_else(|| Some(raw.to_string()));
            }
        }

        return None;
    }

    None
}

fn extract_json_string_field_lossy(arguments: &str, key: &str) -> Option<String> {
    if let Some(value) = extract_json_string_field(arguments, key) {
        return Some(value);
    }

    let quoted_key = format!("\"{key}\"");
    let mut search_from = 0usize;

    while let Some(relative_pos) = arguments[search_from..].find(&quoted_key) {
        let mut cursor = search_from + relative_pos + quoted_key.len();
        cursor = skip_json_whitespace(arguments, cursor);
        if arguments[cursor..].chars().next()? != ':' {
            search_from = cursor;
            continue;
        }
        cursor += ':'.len_utf8();
        cursor = skip_json_whitespace(arguments, cursor);
        if arguments[cursor..].chars().next()? != '"' {
            search_from = cursor;
            continue;
        }
        cursor += '"'.len_utf8();

        let mut escaped = false;
        for (offset, ch) in arguments[cursor..].char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                let end = cursor + offset;
                return decode_json_string_fragment_lossy(&arguments[cursor..end]);
            }
        }

        return decode_json_string_fragment_lossy(&arguments[cursor..]);
    }

    None
}

fn decode_json_string_fragment_lossy(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }

    let mut decoded = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            decoded.push(ch);
            continue;
        }

        match chars.next() {
            Some('"') => decoded.push('"'),
            Some('\\') => decoded.push('\\'),
            Some('/') => decoded.push('/'),
            Some('b') => decoded.push('\u{0008}'),
            Some('f') => decoded.push('\u{000C}'),
            Some('n') => decoded.push('\n'),
            Some('r') => decoded.push('\r'),
            Some('t') => decoded.push('\t'),
            Some('u') => {
                let mut hex = String::with_capacity(4);
                for _ in 0..4 {
                    if let Some(h) = chars.next() {
                        hex.push(h);
                    }
                }
                if hex.len() == 4
                    && let Ok(code) = u32::from_str_radix(&hex, 16)
                    && let Some(c) = char::from_u32(code)
                {
                    decoded.push(c);
                }
            }
            Some(other) => decoded.push(other),
            None => break,
        }
    }

    Some(decoded)
}

fn partial_write_input(file_path: String, content: String) -> Option<Value> {
    if file_path.trim().is_empty() || content.trim().is_empty() {
        return None;
    }

    Some(json!({
        "file_path": file_path,
        "content": content,
        RECOVERED_PARTIAL_WRITE_KEY: true
    }))
}

fn partial_write_input_from_jsonish(arguments: &str) -> Option<Value> {
    let file_path = extract_json_string_field_lossy(arguments, "file_path")
        .or_else(|| extract_json_string_field_lossy(arguments, "filePath"))
        .or_else(|| extract_json_string_field_lossy(arguments, "path"))?;
    let content = extract_json_string_field_lossy(arguments, "content")?;
    partial_write_input(file_path, content)
}

fn partial_write_input_from_text_block(block: &str) -> Option<Value> {
    if block.starts_with('{') {
        let name = extract_json_string_field_lossy(block, "name")?;
        if name.trim() != "Write" {
            return None;
        }
        return partial_write_input_from_jsonish(block);
    }

    let name = str_between(block, "<function=", ">")?.trim();
    if name != "Write" {
        return None;
    }
    let file_path = extract_xml_parameter_text(block, "file_path")
        .or_else(|| extract_xml_parameter_text(block, "filePath"))
        .or_else(|| extract_xml_parameter_text(block, "path"))?;
    let content = extract_xml_parameter_text(block, "content")?;
    partial_write_input(file_path, content)
}

fn recover_length_structured_tool_calls(
    state: &mut StreamState,
    auto_tool_id: bool,
) -> Vec<LlmEvent> {
    let mut events = Vec::new();

    for tc in state.tool_calls.drain(..) {
        if tc.name.trim().is_empty() {
            continue;
        }
        let id = if tc.id.is_empty() && auto_tool_id {
            generate_call_id()
        } else {
            tc.id
        };
        if id.trim().is_empty() {
            continue;
        }

        if let Ok(input) = serde_json::from_str::<Value>(&tc.arguments) {
            events.push(LlmEvent::ToolUse {
                id,
                name: tc.name,
                input,
                extra: tc.extra,
            });
            continue;
        }

        if tc.name == "Write"
            && let Some(input) = partial_write_input_from_jsonish(&tc.arguments)
        {
            events.push(LlmEvent::ToolUse {
                id,
                name: tc.name,
                input,
                extra: tc.extra,
            });
        }
    }

    events
}

fn text_tool_call_blocks(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<tool_call>") {
        let after = &rest[start + "<tool_call>".len()..];
        match after.find("</tool_call>") {
            Some(end) => {
                out.push((after[..end].to_string(), true));
                rest = &after[end + "</tool_call>".len()..];
            }
            None => {
                out.push((after.to_string(), false));
                break;
            }
        }
    }
    out
}

fn recover_length_text_tool_calls(state: &mut StreamState) -> Vec<LlmEvent> {
    let mut blocks = text_tool_call_blocks(&state.reasoning_buf);
    if blocks.is_empty() {
        blocks = text_tool_call_blocks(&state.content_buf);
    }

    let mut events = Vec::new();
    for (index, (block, closed)) in blocks.into_iter().enumerate() {
        if closed {
            continue;
        }
        let Some(input) = partial_write_input_from_text_block(block.trim()) else {
            continue;
        };
        let progress = state.get_or_create_text_tool(index);
        if progress.id.is_empty() {
            progress.id = generate_call_id();
        }
        events.push(LlmEvent::ToolUse {
            id: progress.id.clone(),
            name: "Write".to_string(),
            input,
            extra: None,
        });
    }

    if events.is_empty() {
        recover_text_tool_calls(state).unwrap_or_default()
    } else {
        events
    }
}

fn skip_json_whitespace(input: &str, mut index: usize) -> usize {
    while let Some(ch) = input[index..].chars().next() {
        if !ch.is_whitespace() {
            break;
        }
        index += ch.len_utf8();
    }
    index
}

fn maybe_tool_progress_event(
    acc: &mut ToolCallAccumulator,
    auto_tool_id: bool,
) -> Option<LlmEvent> {
    if acc.name.trim().is_empty() {
        return None;
    }

    if acc.id.trim().is_empty() {
        if auto_tool_id {
            acc.id = generate_call_id();
        } else {
            return None;
        }
    }

    let input = tool_argument_progress_preview(&acc.arguments);
    let signature = input
        .as_ref()
        .and_then(|value| serde_json::to_string(value).ok())
        .unwrap_or_default();

    if !acc.announced || (!signature.is_empty() && signature != acc.last_progress_signature) {
        acc.announced = true;
        acc.last_progress_signature = signature;
        Some(LlmEvent::ToolUseDelta {
            id: acc.id.clone(),
            name: acc.name.clone(),
            input,
        })
    } else {
        None
    }
}

fn text_tool_progress_events(state: &mut StreamState) -> Vec<LlmEvent> {
    let mut previews = parse_text_tool_call_progress(&state.reasoning_buf);
    if previews.is_empty() {
        previews = parse_text_tool_call_progress(&state.content_buf);
    }

    let mut events = Vec::new();
    for (index, preview) in previews.into_iter().enumerate() {
        let progress = state.get_or_create_text_tool(index);
        if progress.id.is_empty() {
            progress.id = generate_call_id();
        }

        let signature = preview
            .input
            .as_ref()
            .and_then(|value| serde_json::to_string(value).ok())
            .unwrap_or_default();

        if !progress.announced
            || (!signature.is_empty() && signature != progress.last_progress_signature)
        {
            progress.announced = true;
            progress.last_progress_signature = signature;
            events.push(LlmEvent::ToolUseDelta {
                id: progress.id.clone(),
                name: preview.name,
                input: preview.input,
            });
        }
    }

    events
}

fn parse_sse_chunk(data: &str, state: &mut StreamState, auto_tool_id: bool) -> Vec<LlmEvent> {
    let mut events = Vec::new();

    let json: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return events,
    };

    // Extract usage if present
    if let Some(usage) = json.get("usage") {
        let base_prompt = usage["prompt_tokens"]
            .as_u64()
            .unwrap_or(state.input_tokens);

        // DeepSeek-style: prompt_cache_hit_tokens is reported separately and
        // prompt_tokens only contains the cache-miss portion.
        // Add it to get the true total prompt size.
        let cache_hit = usage["prompt_cache_hit_tokens"].as_u64().unwrap_or(0);

        state.input_tokens = base_prompt + cache_hit;
        state.output_tokens = usage["completion_tokens"]
            .as_u64()
            .unwrap_or(state.output_tokens);
    }

    let Some(choice) = json["choices"].as_array().and_then(|c| c.first()) else {
        return events;
    };

    let delta = &choice["delta"];

    // Reasoning content (OpenAI reasoning models)
    if let Some(reasoning) = delta["reasoning_content"].as_str()
        && !reasoning.is_empty()
    {
        let visible_delta = append_raw_text_and_visible_delta(
            &mut state.reasoning_buf,
            &mut state.visible_reasoning_buf,
            reasoning,
        );
        if !visible_delta.is_empty() {
            events.push(LlmEvent::ThinkingDelta(visible_delta));
        }
    }

    // Text content
    if let Some(content) = delta["content"].as_str()
        && !content.is_empty()
    {
        let visible_delta =
            append_raw_text_and_visible_delta(&mut state.content_buf, &mut state.visible_content_buf, content);
        if !visible_delta.is_empty() {
            events.push(LlmEvent::TextDelta(visible_delta));
        }
    }

    // Tool calls
    if let Some(tool_calls) = delta["tool_calls"].as_array() {
        for tc in tool_calls {
            let index = tc["index"].as_u64().unwrap_or(0) as usize;
            let acc = state.get_or_create_tool(index);

            if let Some(id) = tc["id"].as_str() {
                acc.id = id.to_string();
            }
            // Only overwrite when non-empty — some third-party APIs send `"name":""`
            // in every delta chunk which would erase the real name from the first chunk.
            if let Some(name) = tc["function"]["name"].as_str().filter(|n| !n.is_empty()) {
                acc.name = name.to_string();
            }
            if let Some(args) = tc["function"]["arguments"].as_str() {
                acc.arguments.push_str(args);
            }
            if let Some(extra) = tc.get("extra_content").filter(|v| !v.is_null()) {
                acc.extra = Some(extra.clone());
            }
            if let Some(event) = maybe_tool_progress_event(acc, auto_tool_id) {
                events.push(event);
            }
        }
    }

    if state.tool_calls.is_empty() {
        events.extend(text_tool_progress_events(state));
    }

    // Check finish_reason — defer Done until [DONE] so the trailing usage
    // chunk (choices:[]) can update token counts first.
    if let Some(finish_reason) = choice["finish_reason"].as_str() {
        match finish_reason {
            "tool_calls" | "stop" => {
                if !state.tool_calls.is_empty() {
                    // Emit accumulated tool calls. Gemini uses "stop" instead of
                    // "tool_calls" as finish_reason, so we handle both here.
                    for tc in state.tool_calls.drain(..) {
                        let id = if tc.id.is_empty() && auto_tool_id {
                            generate_call_id()
                        } else {
                            tc.id
                        };
                        let input: Value = serde_json::from_str(&tc.arguments)
                            .unwrap_or(Value::Object(serde_json::Map::new()));
                        events.push(LlmEvent::ToolUse {
                            id,
                            name: tc.name,
                            input,
                            extra: tc.extra,
                        });
                    }
                    state.pending_done = Some(LlmEvent::Done {
                        stop_reason: StopReason::ToolUse,
                        usage: TokenUsage::default(),
                    });
                } else if let Some(recovered) = recover_text_tool_calls(state) {
                    // FALLBACK: no STRUCTURED tool calls, but the model emitted a
                    // `<tool_call>…</tool_call>` block in the text/reasoning channel
                    // (Qwen/Hermes format — step reasoning models do this
                    // intermittently under load). Recover + emit it so the turn ACTS
                    // instead of dead-ending with no output (the "卡死" symptom).
                    // Only reached when the structured accumulator is empty, so the
                    // normal path is untouched.
                    for ev in recovered {
                        events.push(ev);
                    }
                    state.pending_done = Some(LlmEvent::Done {
                        stop_reason: StopReason::ToolUse,
                        usage: TokenUsage::default(),
                    });
                } else if finish_reason == "stop" {
                    state.pending_done = Some(LlmEvent::Done {
                        stop_reason: StopReason::EndTurn,
                        usage: TokenUsage::default(),
                    });
                } else {
                    // "tool_calls" with empty accumulator — shouldn't happen,
                    // but treat as ToolUse for safety.
                    state.pending_done = Some(LlmEvent::Done {
                        stop_reason: StopReason::ToolUse,
                        usage: TokenUsage::default(),
                    });
                }
            }
            "length" => {
                let mut recovered = recover_length_structured_tool_calls(state, auto_tool_id);
                if recovered.is_empty() {
                    recovered = recover_length_text_tool_calls(state);
                }
                if recovered.is_empty() {
                    state.pending_done = Some(LlmEvent::Done {
                        stop_reason: StopReason::MaxTokens,
                        usage: TokenUsage::default(),
                    });
                } else {
                    events.extend(recovered);
                    state.pending_done = Some(LlmEvent::Done {
                        stop_reason: StopReason::ToolUse,
                        usage: TokenUsage::default(),
                    });
                }
            }
            _ => {}
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::{parse_sse_chunk, parse_text_tool_calls, StreamState};
    use nomi_types::llm::LlmEvent;
    use nomi_types::message::StopReason;
    use serde_json::json;

    /// Qwen XML form (the exact shape step-3.7-flash emitted in session 18 that
    /// dead-ended): a `<tool_call><function=NAME><parameter=KEY>JSON</parameter>`
    /// block in the reasoning channel → recovered as a structured call.
    #[test]
    fn recovers_qwen_xml_tool_call_from_text() {
        let text = "I will now spawn two sub-agents.<tool_call>\n<function=nomi_spawn>\n<parameter=tasks>\n[{\"name\": \"北京天气\", \"prompt\": \"查北京天气\"}, {\"name\": \"广州天气\", \"prompt\": \"查广州天气\"}]\n</parameter>\n</function>\n</tool_call>";
        let calls = parse_text_tool_calls(text);
        assert_eq!(calls.len(), 1, "one tool call recovered");
        let (name, input) = &calls[0];
        assert_eq!(name, "nomi_spawn");
        let tasks = input.get("tasks").and_then(|v| v.as_array()).expect("tasks array parsed as JSON");
        assert_eq!(tasks.len(), 2, "both tasks parsed");
        assert_eq!(tasks[0]["name"], json!("北京天气"));
    }

    /// Hermes JSON form: `<tool_call>{"name":..,"arguments":{..}}</tool_call>`.
    #[test]
    fn recovers_hermes_json_tool_call_from_text() {
        let text = "<tool_call>{\"name\": \"nomi_run_status\", \"arguments\": {\"run_id\": \"run_x\"}}</tool_call>";
        let calls = parse_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "nomi_run_status");
        assert_eq!(calls[0].1["run_id"], json!("run_x"));
    }

    /// Multiple blocks recovered; plain text with no block yields nothing (no false
    /// positives — normal turns never trigger the fallback).
    #[test]
    fn multiple_blocks_and_no_false_positives() {
        assert!(parse_text_tool_calls("just a normal answer, no tools here").is_empty());
        assert!(parse_text_tool_calls("<tool_call>not json and no function tag</tool_call>").is_empty());
        let two = "<tool_call><function=a><parameter=x>1</parameter></function></tool_call> then <tool_call><function=b><parameter=y>2</parameter></function></tool_call>";
        assert_eq!(parse_text_tool_calls(two).len(), 2);
    }

    /// The buffers default empty and only fill from deltas — a fresh state recovers
    /// nothing (guards against the fallback firing on an empty turn).
    #[test]
    fn empty_state_recovers_nothing() {
        let mut state = StreamState::new();
        assert!(super::recover_text_tool_calls(&mut state).is_none());
    }

    #[test]
    fn text_tool_call_stream_emits_write_preview_before_finish() {
        let mut state = StreamState::new();

        let chunk = r#"{"choices":[{"delta":{"reasoning_content":"<tool_call>{\"name\":\"Write\",\"arguments\":{\"file_path\":\"/tmp/snake.html\",\"content\":\""},"finish_reason":null,"index":0}]}"#;
        let events = parse_sse_chunk(chunk, &mut state, true);

        let progress = events
            .iter()
            .find(|event| matches!(event, LlmEvent::ToolUseDelta { .. }))
            .expect("text-form tool calls should announce running work before the tool block closes");

        if let LlmEvent::ToolUseDelta { name, input, .. } = progress {
            assert_eq!(name, "Write");
            assert_eq!(input.as_ref().unwrap()["file_path"], "/tmp/snake.html");
            assert!(
                input.as_ref().unwrap().get("content").is_none(),
                "large generated file content must not be surfaced as progress input"
            );
        }
    }

    #[test]
    fn text_tool_call_stream_hides_tool_markup_from_visible_thinking() {
        let mut state = StreamState::new();

        let chunk = r#"{"choices":[{"delta":{"reasoning_content":"<tool_call>{\"name\":\"Write\",\"arguments\":{\"file_path\":\"/tmp/snake.html\",\"content\":\""},"finish_reason":null,"index":0}]}"#;
        let events = parse_sse_chunk(chunk, &mut state, true);

        assert!(
            events
                .iter()
                .all(|event| !matches!(event, LlmEvent::ThinkingDelta(_) | LlmEvent::TextDelta(_))),
            "text-form tool call markup should not be streamed as visible assistant or thinking text"
        );
    }

    #[test]
    fn length_finish_recovers_partial_structured_write_tool_call() {
        let mut state = StreamState::new();

        let chunk = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_write","type":"function","function":{"name":"Write","arguments":"{\"file_path\":\"/tmp/index.html\",\"content\":\"<html><body>hello"}}]},"finish_reason":"length","index":0}]}"#;
        let events = parse_sse_chunk(chunk, &mut state, true);

        let recovered = events
            .iter()
            .find_map(|event| match event {
                LlmEvent::ToolUse { id, name, input, .. } => Some((id, name, input)),
                _ => None,
            })
            .expect("length-truncated Write arguments should be recovered as an executable tool call");

        assert_eq!(recovered.0, "call_write");
        assert_eq!(recovered.1, "Write");
        assert_eq!(recovered.2["file_path"], "/tmp/index.html");
        assert_eq!(recovered.2["content"], "<html><body>hello");
        assert_eq!(recovered.2[super::RECOVERED_PARTIAL_WRITE_KEY], true);
        assert!(matches!(
            state.pending_done,
            Some(LlmEvent::Done {
                stop_reason: StopReason::ToolUse,
                ..
            })
        ));
    }

    #[test]
    fn length_finish_recovers_partial_text_write_tool_call() {
        let mut state = StreamState::new();

        let chunk = r#"{"choices":[{"delta":{"reasoning_content":"<tool_call>{\"name\":\"Write\",\"arguments\":{\"file_path\":\"/tmp/index.html\",\"content\":\"<main>hello"},"finish_reason":"length","index":0}]}"#;
        let events = parse_sse_chunk(chunk, &mut state, true);

        let recovered = events
            .iter()
            .find_map(|event| match event {
                LlmEvent::ToolUse { name, input, .. } => Some((name, input)),
                _ => None,
            })
            .expect("length-truncated text-form Write should be recovered as an executable tool call");

        assert_eq!(recovered.0, "Write");
        assert_eq!(recovered.1["file_path"], "/tmp/index.html");
        assert_eq!(recovered.1["content"], "<main>hello");
        assert_eq!(recovered.1[super::RECOVERED_PARTIAL_WRITE_KEY], true);
        assert!(matches!(
            state.pending_done,
            Some(LlmEvent::Done {
                stop_reason: StopReason::ToolUse,
                ..
            })
        ));
    }

    #[test]
    fn text_tool_call_progress_id_is_reused_by_recovered_tool_use() {
        let mut state = StreamState::new();

        let chunk1 = r#"{"choices":[{"delta":{"reasoning_content":"<tool_call>{\"name\":\"Write\",\"arguments\":{\"file_path\":\"/tmp/snake.html\",\"content\":\""},"finish_reason":null,"index":0}]}"#;
        let events1 = parse_sse_chunk(chunk1, &mut state, true);
        let progress_id = events1
            .iter()
            .find_map(|event| match event {
                LlmEvent::ToolUseDelta { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("partial text tool call should announce a progress id");

        let chunk2 = r#"{"choices":[{"delta":{"reasoning_content":"hello\"}}</tool_call>"},"finish_reason":"stop","index":0}]}"#;
        let events2 = parse_sse_chunk(chunk2, &mut state, true);
        let final_id = events2
            .iter()
            .find_map(|event| match event {
                LlmEvent::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("closed text tool call should recover a final ToolUse");

        assert_eq!(progress_id, final_id);
    }

    #[test]
    fn qwen_xml_text_tool_call_stream_emits_file_preview_before_finish() {
        let mut state = StreamState::new();

        let chunk = r#"{"choices":[{"delta":{"reasoning_content":"<tool_call><function=Write><parameter=file_path>/tmp/snake.html</parameter><parameter=content>"},"finish_reason":null,"index":0}]}"#;
        let events = parse_sse_chunk(chunk, &mut state, true);

        let progress = events
            .iter()
            .find(|event| matches!(event, LlmEvent::ToolUseDelta { .. }))
            .expect("Qwen XML text tool calls should announce running work before finish");

        if let LlmEvent::ToolUseDelta { name, input, .. } = progress {
            assert_eq!(name, "Write");
            assert_eq!(input.as_ref().unwrap()["file_path"], "/tmp/snake.html");
        }
    }

    #[tokio::test]
    async fn stream_without_done_sentinel_still_emits_done() {
        use super::{StreamOutcome, process_sse_stream};
        // Some OpenAI-compatible servers (vLLM, local deployments) close the
        // connection after the final chunk without sending the `[DONE]`
        // sentinel. The parser must still flush the deferred Done so the
        // consumer gets a terminal event instead of hanging. (Phase 1)
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        );
        let http_resp = http::Response::builder()
            .status(200)
            .body(body.to_string())
            .unwrap();
        let response = reqwest::Response::from(http_resp);

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let outcome = process_sse_stream(response, &tx, false).await;
        drop(tx);

        let mut saw_done = false;
        while let Some(ev) = rx.recv().await {
            if matches!(ev, LlmEvent::Done { .. }) {
                saw_done = true;
            }
        }
        assert!(saw_done, "stream ending without [DONE] must still emit a Done");
        assert!(matches!(outcome, StreamOutcome::Ok));
    }

    #[test]
    fn tool_images_ride_in_follow_up_user_message() {
        use nomi_types::message::{ContentBlock, Message, Role};
        let messages = vec![Message::new(
            Role::Tool,
            vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "screenshot taken".to_string(),
                is_error: false,
                images: vec![nomi_types::tool::ToolImage {
                    media_type: "image/png".to_string(),
                    data: "aGVsbG8=".to_string(),
                }],
            }],
        )];
        let compat = nomi_config::compat::ProviderCompat::openai_defaults();
        let result = OpenAIProvider::build_messages(&messages, "", &compat);
        // tool message first, then a user message carrying the image
        assert_eq!(result[0]["role"], "tool");
        assert_eq!(result[0]["content"], "screenshot taken");
        assert_eq!(result[1]["role"], "user");
        let parts = result[1]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert!(
            parts[1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
    }

    #[test]
    fn user_message_image_block_produces_image_url_content() {
        use nomi_types::message::{ContentBlock, Message, Role};
        let messages = vec![Message::new(
            Role::User,
            vec![
                ContentBlock::Text {
                    text: "Describe this image".to_string(),
                },
                ContentBlock::Image {
                    media_type: "image/png".to_string(),
                    data: "aGVsbG8=".to_string(),
                },
            ],
        )];
        let compat = nomi_config::compat::ProviderCompat::openai_defaults();
        let result = OpenAIProvider::build_messages(&messages, "", &compat);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["role"], "user");
        let content = result[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "Describe this image");
        assert_eq!(content[1]["type"], "image_url");
        assert!(
            content[1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert!(
            content[1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .ends_with("aGVsbG8=")
        );
    }

    #[test]
    fn strips_user_image_when_supports_image_false() {
        use nomi_types::message::{ContentBlock, Message, Role};
        let compat = ProviderCompat {
            supports_image: Some(false),
            ..Default::default()
        };
        let messages = vec![Message::new(
            Role::User,
            vec![
                ContentBlock::Text { text: "看这张图".into() },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                },
            ],
        )];
        let out = OpenAIProvider::build_messages(&messages, "", &compat);
        let s = serde_json::to_string(&out).unwrap();
        assert!(!s.contains("image_url"), "不应出现 image_url: {s}");
        assert!(s.contains("图片已省略"), "应出现占位: {s}");
    }

    #[test]
    fn keeps_user_image_when_supports_image_true() {
        use nomi_types::message::{ContentBlock, Message, Role};
        let compat = ProviderCompat::default(); // supports_image() == true
        let messages = vec![Message::new(
            Role::User,
            vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            }],
        )];
        let out = OpenAIProvider::build_messages(&messages, "", &compat);
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains("image_url"), "应保留 image_url: {s}");
    }

    use super::*;

    fn no_compat() -> ProviderCompat {
        ProviderCompat::default()
    }

    fn openai_compat() -> ProviderCompat {
        ProviderCompat::openai_defaults()
    }

    fn simple_request() -> LlmRequest {
        LlmRequest {
            model: "gpt-4o-mini".into(),
            system: String::new(),
            messages: vec![Message::new(
                Role::User,
                vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
            )],
            tools: vec![],
            max_tokens: 16,
            thinking: None,
            reasoning_effort: None,
        }
    }

    async fn drain_stream(mut rx: tokio::sync::mpsc::Receiver<LlmEvent>) {
        while rx.recv().await.is_some() {}
    }

    #[tokio::test]
    async fn stream_builds_http_client_for_each_call() {
        use crate::{http_client_build_count, reset_http_client_build_count};
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .expect(2)
            .mount(&server)
            .await;

        let provider = OpenAIProvider::new("key", &server.uri(), openai_compat());
        reset_http_client_build_count();

        drain_stream(provider.stream(&simple_request()).await.unwrap()).await;
        assert_eq!(http_client_build_count(), 1);

        drain_stream(provider.stream(&simple_request()).await.unwrap()).await;
        assert_eq!(http_client_build_count(), 2);
    }

    // --- max_tokens_field ---

    #[test]
    fn test_max_tokens_field_default() {
        let provider = OpenAIProvider::new("key", "http://localhost", openai_compat());
        let req = LlmRequest {
            model: "gpt-4o".into(),
            system: String::new(),
            messages: vec![],
            tools: vec![],
            max_tokens: 1024,
            thinking: None,
            reasoning_effort: None,
        };
        let body = provider.build_request_body(&req);
        assert_eq!(body["max_tokens"], 1024);
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn test_max_tokens_field_custom() {
        let compat = ProviderCompat {
            max_tokens_field: Some("max_completion_tokens".into()),
            ..Default::default()
        };
        let provider = OpenAIProvider::new("key", "http://localhost", compat);
        let req = LlmRequest {
            model: "gpt-4o".into(),
            system: String::new(),
            messages: vec![],
            tools: vec![],
            max_tokens: 2048,
            thinking: None,
            reasoning_effort: None,
        };
        let body = provider.build_request_body(&req);
        assert_eq!(body["max_completion_tokens"], 2048);
        assert!(body.get("max_tokens").is_none());
    }

    // --- merge_assistant_messages ---

    #[test]
    fn test_merge_assistant_messages_enabled() {
        let messages = vec![
            Message::new(
                Role::Assistant,
                vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
            ),
            Message::new(
                Role::Assistant,
                vec![ContentBlock::Text {
                    text: " world".into(),
                }],
            ),
        ];
        let result = OpenAIProvider::build_messages(&messages, "", &openai_compat());
        let assistant_msgs: Vec<_> = result.iter().filter(|m| m["role"] == "assistant").collect();
        assert_eq!(assistant_msgs.len(), 1);
        assert_eq!(assistant_msgs[0]["content"], "hello world");
    }

    #[test]
    fn test_merge_assistant_messages_disabled() {
        let messages = vec![
            Message::new(
                Role::Assistant,
                vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
            ),
            Message::new(
                Role::Assistant,
                vec![ContentBlock::Text {
                    text: " world".into(),
                }],
            ),
        ];
        let result = OpenAIProvider::build_messages(&messages, "", &no_compat());
        let assistant_msgs: Vec<_> = result.iter().filter(|m| m["role"] == "assistant").collect();
        assert_eq!(assistant_msgs.len(), 2);
    }

    // --- clean_orphan_tool_calls ---

    #[test]
    fn test_clean_orphan_tool_calls_enabled() {
        let messages = vec![
            Message::new(
                Role::Assistant,
                vec![
                    ContentBlock::ToolUse {
                        id: "tc1".into(),
                        name: "bash".into(),
                        input: json!({}),
                        extra: None,
                    },
                    ContentBlock::ToolUse {
                        id: "tc2".into(),
                        name: "read".into(),
                        input: json!({}),
                        extra: None,
                    },
                ],
            ),
            Message::new(
                Role::Tool,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "tc1".into(),
                    content: "ok".into(),
                    is_error: false,
                    images: Vec::new(),
                }],
            ),
            // tc2 has no result -> orphan
        ];
        let result = OpenAIProvider::build_messages(&messages, "", &openai_compat());
        let assistant = result.iter().find(|m| m["role"] == "assistant").unwrap();
        let tcs = assistant["tool_calls"].as_array().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0]["id"], "tc1");
    }

    #[test]
    fn test_clean_orphan_tool_calls_disabled() {
        let messages = vec![
            Message::new(
                Role::Assistant,
                vec![
                    ContentBlock::ToolUse {
                        id: "tc1".into(),
                        name: "bash".into(),
                        input: json!({}),
                        extra: None,
                    },
                    ContentBlock::ToolUse {
                        id: "tc2".into(),
                        name: "read".into(),
                        input: json!({}),
                        extra: None,
                    },
                ],
            ),
            Message::new(
                Role::Tool,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "tc1".into(),
                    content: "ok".into(),
                    is_error: false,
                    images: Vec::new(),
                }],
            ),
        ];
        let result = OpenAIProvider::build_messages(&messages, "", &no_compat());
        let assistant = result.iter().find(|m| m["role"] == "assistant").unwrap();
        let tcs = assistant["tool_calls"].as_array().unwrap();
        assert_eq!(tcs.len(), 2);
    }

    // --- dedup_tool_results ---

    #[test]
    fn test_dedup_tool_results_enabled() {
        let messages = vec![
            Message::new(
                Role::Assistant,
                vec![ContentBlock::ToolUse {
                    id: "tc1".into(),
                    name: "bash".into(),
                    input: json!({}),
                    extra: None,
                }],
            ),
            Message::new(
                Role::Tool,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "tc1".into(),
                    content: "first".into(),
                    is_error: false,
                    images: Vec::new(),
                }],
            ),
            Message::new(
                Role::Tool,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "tc1".into(),
                    content: "second".into(),
                    is_error: false,
                    images: Vec::new(),
                }],
            ),
        ];
        let result = OpenAIProvider::build_messages(&messages, "", &openai_compat());
        let tool_msgs: Vec<_> = result.iter().filter(|m| m["role"] == "tool").collect();
        assert_eq!(tool_msgs.len(), 1);
        assert_eq!(tool_msgs[0]["content"], "second");
    }

    // --- usage token parsing ---

    #[test]
    fn test_usage_from_trailing_chunk() {
        // OpenAI sends usage in a trailing chunk where choices:[] — the Done
        // event must carry the token counts from that chunk, not zeros.
        let mut state = StreamState::new();

        // chunk 1: finish_reason + text delta, no usage
        let chunk1 = r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
        let events = parse_sse_chunk(chunk1, &mut state, false);
        // TextDelta is emitted immediately; Done is deferred.
        assert!(
            events.iter().all(|e| !matches!(e, LlmEvent::Done { .. })),
            "Done should be deferred, not emitted with finish_reason chunk"
        );
        assert!(state.pending_done.is_some());

        // chunk 2: trailing usage-only chunk (choices:[])
        let chunk2 = r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
        let events2 = parse_sse_chunk(chunk2, &mut state, false);
        assert!(events2.is_empty());
        assert_eq!(state.input_tokens, 10);
        assert_eq!(state.output_tokens, 5);

        // [DONE] — flush with final counts
        let done = state.flush_done().expect("pending_done should be Some");
        match done {
            LlmEvent::Done { stop_reason, usage } => {
                assert_eq!(stop_reason, StopReason::EndTurn);
                assert_eq!(usage.input_tokens, 10);
                assert_eq!(usage.output_tokens, 5);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn test_usage_in_finish_chunk() {
        // Some providers/models include usage in the same chunk as finish_reason.
        // Counts should still be correct after flush.
        let mut state = StreamState::new();

        // No text delta here, only finish_reason + usage in the same chunk.
        let chunk = r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":8,"completion_tokens":3}}"#;
        let events = parse_sse_chunk(chunk, &mut state, false);
        assert!(
            events.iter().all(|e| !matches!(e, LlmEvent::Done { .. })),
            "Done should be deferred even when usage is in the finish chunk"
        );
        assert_eq!(state.output_tokens, 3);

        let done = state.flush_done().unwrap();
        match done {
            LlmEvent::Done { usage, .. } => {
                assert_eq!(usage.output_tokens, 3);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn test_build_tools_deferred_has_empty_parameters() {
        let tools = vec![
            ToolDef {
                name: "Read".into(),
                description: "Read a file".into(),
                input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
                deferred: false,
            },
            ToolDef {
                name: "SpawnTool".into(),
                description: "Spawn sub-agents".into(),
                input_schema: json!({"type": "object", "properties": {"agents": {"type": "array"}}}),
                deferred: true,
            },
        ];
        let result = OpenAIProvider::build_tools(&tools);

        // Core tool has full parameters
        let read_params = &result[0]["function"]["parameters"];
        assert!(read_params["properties"].get("path").is_some());

        // Deferred tool has empty parameters and modified description
        let spawn_params = &result[1]["function"]["parameters"];
        assert!(spawn_params["properties"].as_object().unwrap().is_empty());
        let spawn_desc = result[1]["function"]["description"].as_str().unwrap();
        assert!(spawn_desc.contains("ToolSearch"));
    }

    #[test]
    fn usage_includes_prompt_cache_hit_tokens() {
        // DeepSeek reports prompt_cache_hit_tokens separately;
        // input_tokens should be the sum of prompt_tokens + prompt_cache_hit_tokens
        let mut state = StreamState::new();

        let chunk = r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":500,"completion_tokens":100,"prompt_cache_hit_tokens":999500}}"#;
        let _ = parse_sse_chunk(chunk, &mut state, false);

        assert_eq!(state.input_tokens, 1_000_000);
        assert_eq!(state.output_tokens, 100);
    }

    #[test]
    fn usage_with_prompt_tokens_details_cached() {
        // OpenAI standard: prompt_tokens already includes cached_tokens (it's the total)
        // prompt_tokens_details.cached_tokens is informational only
        let mut state = StreamState::new();

        let chunk = r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1000000,"completion_tokens":100,"prompt_tokens_details":{"cached_tokens":999000}}}"#;
        let _ = parse_sse_chunk(chunk, &mut state, false);

        // prompt_tokens is already the full total for OpenAI
        assert_eq!(state.input_tokens, 1_000_000);
        assert_eq!(state.output_tokens, 100);
    }

    #[test]
    fn usage_without_cache_fields_unchanged() {
        // Provider that only sends prompt_tokens (no cache fields)
        let mut state = StreamState::new();

        let chunk = r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":50000,"completion_tokens":200}}"#;
        let _ = parse_sse_chunk(chunk, &mut state, false);

        assert_eq!(state.input_tokens, 50_000);
        assert_eq!(state.output_tokens, 200);
    }

    #[test]
    fn tool_calls_with_stop_finish_reason() {
        // Gemini uses finish_reason:"stop" even when tool_calls are present.
        // The accumulated tool calls must still be emitted, and the first
        // structured delta should be visible immediately so the UI is not blank
        // while long arguments are still being generated.
        let mut state = StreamState::new();

        // chunk 1: tool call delta (name + partial args)
        let chunk1 = r#"{"choices":[{"delta":{"role":"assistant","tool_calls":[{"extra_content":{},"function":{"arguments":"{\"skill\":\"test\",\"args\":\"hello\"}","name":"Skill"},"id":"call_abc123","type":"function"}]},"index":0}]}"#;
        let events1 = parse_sse_chunk(chunk1, &mut state, false);
        let progress = events1
            .iter()
            .find(|e| matches!(e, LlmEvent::ToolUseDelta { .. }))
            .expect("tool call deltas should announce running work before finish_reason");
        if let LlmEvent::ToolUseDelta { id, name, input } = progress {
            assert_eq!(id, "call_abc123");
            assert_eq!(name, "Skill");
            assert_eq!(input.as_ref().unwrap()["skill"], "test");
        }
        assert_eq!(state.tool_calls.len(), 1);
        assert_eq!(state.tool_calls[0].name, "Skill");

        // chunk 2: finish_reason:"stop" (not "tool_calls")
        let chunk2 = r#"{"choices":[{"delta":{"role":"assistant"},"finish_reason":"stop","index":0}],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}}"#;
        let events2 = parse_sse_chunk(chunk2, &mut state, false);

        // Tool call should be emitted
        let tool_events: Vec<_> = events2
            .iter()
            .filter(|e| matches!(e, LlmEvent::ToolUse { .. }))
            .collect();
        assert_eq!(tool_events.len(), 1, "tool call should be emitted on stop");
        if let LlmEvent::ToolUse {
            id, name, input, ..
        } = &tool_events[0]
        {
            assert_eq!(id, "call_abc123");
            assert_eq!(name, "Skill");
            assert_eq!(input["skill"], "test");
        }

        // Done should be deferred with ToolUse stop reason
        let done = state.flush_done().unwrap();
        match done {
            LlmEvent::Done { stop_reason, .. } => {
                assert_eq!(stop_reason, StopReason::ToolUse);
            }
            other => panic!("expected Done with ToolUse, got {other:?}"),
        }

        assert!(state.tool_calls.is_empty(), "tool calls should be drained");
    }

    #[test]
    fn tool_call_argument_stream_emits_file_target_preview_before_finish() {
        let mut state = StreamState::new();

        let chunk = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_write_1","function":{"name":"Write","arguments":"{\"file_path\":\"/tmp/snake.html\",\"content\":\""}}]},"finish_reason":null,"index":0}]}"#;
        let events = parse_sse_chunk(chunk, &mut state, false);

        let progress = events
            .iter()
            .find(|e| matches!(e, LlmEvent::ToolUseDelta { .. }))
            .expect("Write should be announced while arguments are still streaming");
        if let LlmEvent::ToolUseDelta { id, name, input } = progress {
            assert_eq!(id, "call_write_1");
            assert_eq!(name, "Write");
            assert_eq!(input.as_ref().unwrap()["file_path"], "/tmp/snake.html");
            assert!(
                input.as_ref().unwrap().get("content").is_none(),
                "large write content must not be pushed as a progress preview"
            );
        }
    }

    #[test]
    fn auto_tool_id_is_stable_between_progress_and_final_tool_use() {
        let mut state = StreamState::new();

        let chunk1 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"Bash","arguments":"{\"command\":\"bun test\""}}]},"finish_reason":null,"index":0}]}"#;
        let events1 = parse_sse_chunk(chunk1, &mut state, true);
        let progress_id = events1
            .iter()
            .find_map(|e| match e {
                LlmEvent::ToolUseDelta { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("auto-id providers should still emit a stable progress event");

        let chunk2 = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls","index":0}]}"#;
        let events2 = parse_sse_chunk(chunk2, &mut state, true);
        let final_id = events2
            .iter()
            .find_map(|e| match e {
                LlmEvent::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("final tool use should be emitted");

        assert_eq!(progress_id, final_id);
    }

    #[test]
    fn stop_without_tool_calls_unchanged() {
        // Standard stop without tool calls should still produce EndTurn.
        let mut state = StreamState::new();

        let chunk =
            r#"{"choices":[{"delta":{"content":"done"},"finish_reason":"stop","index":0}]}"#;
        let events = parse_sse_chunk(chunk, &mut state, false);

        let text_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, LlmEvent::TextDelta(_)))
            .collect();
        assert_eq!(text_events.len(), 1);

        let done = state.flush_done().unwrap();
        match done {
            LlmEvent::Done { stop_reason, .. } => {
                assert_eq!(stop_reason, StopReason::EndTurn);
            }
            other => panic!("expected Done with EndTurn, got {other:?}"),
        }
    }

    #[test]
    fn test_auto_tool_id_generates_id_when_empty() {
        let mut state = StreamState::new();

        // Simulate a provider that returns tool_calls without an id field
        let chunk = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"get_weather","arguments":"{\"city\":\"Beijing\"}"}}]},"finish_reason":"tool_calls","index":0}]}"#;
        let events = parse_sse_chunk(chunk, &mut state, true);

        let tool_use = events
            .iter()
            .find(|e| matches!(e, LlmEvent::ToolUse { .. }))
            .expect("should emit ToolUse event");

        if let LlmEvent::ToolUse { id, name, .. } = tool_use {
            assert!(!id.is_empty(), "id should be auto-generated, not empty");
            assert!(id.starts_with("call_"), "id should have call_ prefix");
            assert_eq!(name, "get_weather");
        }
    }

    #[test]
    fn test_auto_tool_id_preserves_existing_id() {
        let mut state = StreamState::new();

        let chunk = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_existing_123","function":{"name":"read_file","arguments":"{}"}}]},"finish_reason":"tool_calls","index":0}]}"#;
        let events = parse_sse_chunk(chunk, &mut state, true);

        let tool_use = events
            .iter()
            .find(|e| matches!(e, LlmEvent::ToolUse { .. }))
            .expect("should emit ToolUse event");

        if let LlmEvent::ToolUse { id, .. } = tool_use {
            assert_eq!(id, "call_existing_123", "existing id should be preserved");
        }
    }

    #[test]
    fn test_auto_tool_id_disabled_keeps_empty() {
        let mut state = StreamState::new();

        let chunk = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"get_weather","arguments":"{}"}}]},"finish_reason":"tool_calls","index":0}]}"#;
        let events = parse_sse_chunk(chunk, &mut state, false);

        let tool_use = events
            .iter()
            .find(|e| matches!(e, LlmEvent::ToolUse { .. }))
            .expect("should emit ToolUse event");

        if let LlmEvent::ToolUse { id, .. } = tool_use {
            assert!(
                id.is_empty(),
                "id should remain empty when auto_tool_id is disabled"
            );
        }
    }
}

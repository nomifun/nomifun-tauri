use nomifun_common::{Confirmation, ConfirmationOption};
use serde_json::Value;
use tracing::debug;

use super::protocol::{AgentEvent, ApprovalRequestedEvent, ChatEvent, ChatEventState, EventFrame};
use crate::protocol::events::{
    AcpPermissionEventData, AgentStreamEvent, ErrorEventData, FinishEventData, StartEventData, TextEventData,
    ThinkingEventData, ToolCallEventData, ToolCallStatus, TurnStopReason,
};

#[derive(Default)]
pub struct TextFallbackState {
    pub accumulated_text: String,
    pub agent_assistant_fallback: String,
    pub turn_active: bool,
    pub current_msg_id: Option<String>,
    pub current_run_id: Option<String>,
    pub current_session_key: Option<String>,
}

impl TextFallbackState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset_for_new_turn(&mut self) {
        self.accumulated_text.clear();
        self.agent_assistant_fallback.clear();
        self.turn_active = true;
        self.current_msg_id = None;
        self.current_run_id = None;
    }
}

pub fn map_openclaw_event(
    event: &EventFrame,
    text_state: &mut TextFallbackState,
    our_session_key: Option<&str>,
) -> Vec<AgentStreamEvent> {
    let event_name = event.event.as_str();

    match event_name {
        "chat" | "chat.event" => map_chat_event(event, text_state, our_session_key),
        "agent" | "agent.event" => map_agent_event(event, text_state, our_session_key),
        "exec.approval.requested" => map_approval_event(event, our_session_key),
        // Durable session approval replay/terminal events require an explicit
        // sessions.messages.subscribe(includeApprovals=true) lifecycle. Do not
        // treat them as legacy approvals until that state machine is wired.
        "session.approval" => vec![],
        "tick" | "health" | "shutdown" | "connect.challenge" => vec![],
        _ => {
            debug!(event = event_name, "Unhandled OpenClaw event type");
            vec![]
        }
    }
}

fn map_chat_event(
    event: &EventFrame,
    text_state: &mut TextFallbackState,
    our_session_key: Option<&str>,
) -> Vec<AgentStreamEvent> {
    let payload = match event.payload.as_ref() {
        Some(p) => p,
        None => return vec![],
    };

    let chat: ChatEvent = match serde_json::from_value(payload.clone()) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    if is_from_other_session(chat.session_key.as_deref(), our_session_key) {
        return vec![];
    }

    if is_from_other_run(chat.run_id.as_deref(), text_state.current_run_id.as_deref()) {
        return vec![];
    }
    if text_state.current_run_id.is_none() {
        text_state.current_run_id = chat.run_id.clone();
    }
    if let Some(ref sk) = chat.session_key {
        text_state.current_session_key = Some(sk.clone());
    }

    let mut events = Vec::new();

    match chat.state {
        ChatEventState::Delta => {
            if !text_state.turn_active {
                text_state.reset_for_new_turn();
                text_state.current_run_id = chat.run_id.clone();
                events.push(AgentStreamEvent::Start(StartEventData {
                    session_id: chat.session_key.clone(),
                }));
            }

            // v4 schema delivers the incremental chunk in `deltaText` (with optional
            // `replace=true` meaning the whole accumulated text should be reset to it).
            // v3 schema instead sends cumulative text on `message` — we diff it.
            let delta = if let Some(delta_text) = chat.delta_text.as_deref() {
                if chat.replace == Some(true) {
                    // `replace` carries the complete current assistant text, not
                    // an append-only chunk. NomiFun's AgentStreamEvent::Text is
                    // append-only, so emit only the suffix when possible. If
                    // the replacement rewrites earlier text, suppress it here
                    // rather than duplicating the full response in the relay.
                    let emitted = suffix_after_prefix(&text_state.accumulated_text, delta_text);
                    text_state.accumulated_text = delta_text.to_owned();
                    emitted
                } else {
                    text_state.accumulated_text.push_str(delta_text);
                    (!delta_text.is_empty()).then(|| delta_text.to_owned())
                }
            } else {
                compute_text_delta(&chat.message, &mut text_state.accumulated_text)
            };

            if let Some(delta) = delta {
                if text_state.current_msg_id.is_none() {
                    text_state.current_msg_id = Some(uuid::Uuid::new_v4().to_string());
                }
                events.push(AgentStreamEvent::Text(TextEventData { content: delta }));
            }
        }
        ChatEventState::Final => {
            if text_state.accumulated_text.is_empty() && !text_state.agent_assistant_fallback.is_empty() {
                // Layer 2 fallback: use agent.assistant buffered text
                if text_state.current_msg_id.is_none() {
                    text_state.current_msg_id = Some(uuid::Uuid::new_v4().to_string());
                }
                events.push(AgentStreamEvent::Text(TextEventData {
                    content: text_state.agent_assistant_fallback.clone(),
                }));
            }

            events.push(AgentStreamEvent::Finish(FinishEventData {
                session_id: chat.session_key,
                stop_reason: Some(TurnStopReason::EndTurn),
            }));
            text_state.turn_active = false;
        }
        ChatEventState::Aborted => {
            events.push(AgentStreamEvent::Finish(FinishEventData {
                session_id: chat.session_key,
                stop_reason: Some(TurnStopReason::Cancelled),
            }));
            text_state.turn_active = false;
        }
        ChatEventState::Error => {
            if text_state.accumulated_text.is_empty()
                && let Some(content) = chat.message.as_ref().and_then(extract_text_from_message)
                && !content.is_empty()
            {
                text_state.accumulated_text = content.clone();
                events.push(AgentStreamEvent::Text(TextEventData { content }));
            }
            let msg = chat.error_message.unwrap_or_else(|| "Unknown chat error".into());
            events.push(AgentStreamEvent::Error(ErrorEventData::legacy(msg, None)));
            text_state.turn_active = false;
        }
    }

    events
}

fn map_agent_event(
    event: &EventFrame,
    text_state: &mut TextFallbackState,
    our_session_key: Option<&str>,
) -> Vec<AgentStreamEvent> {
    let payload = match event.payload.as_ref() {
        Some(p) => p,
        None => return vec![],
    };

    let agent_evt: AgentEvent = match serde_json::from_value(payload.clone()) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let event_session_key = agent_evt
        .session_key
        .as_deref()
        .or_else(|| agent_evt.data.get("sessionKey").and_then(Value::as_str));
    if is_from_other_session(event_session_key, our_session_key) {
        return vec![];
    }
    if is_from_other_run(agent_evt.run_id.as_deref(), text_state.current_run_id.as_deref()) {
        return vec![];
    }
    if text_state.current_run_id.is_none() {
        text_state.current_run_id = agent_evt.run_id.clone();
    }

    let stream = agent_evt.stream.as_str();
    let data = &agent_evt.data;

    match stream {
        "thinking" | "thought" => {
            let content = data.get("text").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            if content.is_empty() {
                return vec![];
            }
            vec![AgentStreamEvent::Thinking(ThinkingEventData {
                content,
                subject: data.get("subject").and_then(|v| v.as_str()).map(String::from),
                duration: None,
                status: Some("in_progress".into()),
            })]
        }
        "tool" | "tool_call" => map_tool_event(data),
        "assistant" => {
            // Layer 2 buffer: accumulate for fallback
            if let Some(text) = data.get("text").and_then(|v| v.as_str()) {
                text_state.agent_assistant_fallback.push_str(text);
            }
            vec![]
        }
        // Turn lifecycle is driven by chat.state events (final/aborted/error)
        "lifecycle" => vec![],
        _ => {
            debug!(stream = stream, "Unhandled agent event stream");
            vec![]
        }
    }
}

fn map_tool_event(data: &Value) -> Vec<AgentStreamEvent> {
    let phase = data.get("phase").and_then(|v| v.as_str()).unwrap_or("");
    let is_error = data.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);

    let status = match phase {
        "result" if is_error => ToolCallStatus::Error,
        "result" => ToolCallStatus::Completed,
        _ => ToolCallStatus::Running,
    };

    let call_id = data.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let name = data.get("name").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let args = data.get("args").cloned().unwrap_or_default();

    vec![AgentStreamEvent::ToolCall(ToolCallEventData {
        call_id,
        name,
        args,
        status,
        input: None,
        output: None,
        description: None,
    })]
}

fn map_approval_event(event: &EventFrame, our_session_key: Option<&str>) -> Vec<AgentStreamEvent> {
    let payload = match event.payload.as_ref() {
        Some(p) => p,
        None => return vec![],
    };

    let approval: ApprovalRequestedEvent = match serde_json::from_value(payload.clone()) {
        Ok(a) => a,
        Err(_) => return vec![],
    };

    let request = approval.request.unwrap_or(approval.direct_request);
    if is_from_other_session(request.session_key.as_deref(), our_session_key) {
        return vec![];
    }
    let command = request
        .command
        .clone()
        .or(request.command_preview.clone())
        .unwrap_or_else(|| "Remote command".into());
    let description = request
        .warning_text
        .clone()
        .or(request.command_preview.clone())
        .unwrap_or_else(|| command.clone());

    let options = request
        .allowed_decisions
        .unwrap_or_else(|| vec!["allow-once".into(), "allow-always".into(), "deny".into()])
        .into_iter()
        .map(|decision| ConfirmationOption {
            label: decision.clone(),
            value: Value::String(decision),
            params: None,
        })
        .collect();

    let confirmation = Confirmation {
        id: approval.id.clone(),
        call_id: approval.id,
        title: Some(command.clone()),
        action: Some(command),
        description,
        command_type: request.host,
        options,
        screenshot: None,
    };

    vec![AgentStreamEvent::AcpPermission(AcpPermissionEventData::Confirmation(
        confirmation,
    ))]
}

fn compute_text_delta(message: &Option<Value>, accumulated: &mut String) -> Option<String> {
    let msg = message.as_ref()?;
    let cumulative_text = extract_text_from_message(msg)?;

    let delta = suffix_after_prefix(accumulated, &cumulative_text);
    *accumulated = cumulative_text;
    delta
}

fn suffix_after_prefix(previous: &str, current: &str) -> Option<String> {
    current
        .strip_prefix(previous)
        .filter(|suffix| !suffix.is_empty())
        .map(ToOwned::to_owned)
}

fn extract_text_from_message(message: &Value) -> Option<String> {
    // Format 1: { content: "string" }
    if let Some(s) = message.get("content").and_then(|v| v.as_str()) {
        return Some(s.to_owned());
    }

    // Format 2: { content: [{ type: "text", text: "..." }, ...] }
    if let Some(blocks) = message.get("content").and_then(|v| v.as_array()) {
        let text: String = blocks
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    b.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect();
        if !text.is_empty() {
            return Some(text);
        }
    }

    // Format 3: { text: "string" }
    if let Some(s) = message.get("text").and_then(|v| v.as_str()) {
        return Some(s.to_owned());
    }

    None
}

fn is_from_other_session(event_session: Option<&str>, our_session: Option<&str>) -> bool {
    match (event_session, our_session) {
        (Some(event_sk), Some(our_sk)) => event_sk != our_sk,
        _ => false,
    }
}

fn is_from_other_run(event_run: Option<&str>, current_run: Option<&str>) -> bool {
    match (event_run, current_run) {
        (Some(event_run), Some(current_run)) => event_run != current_run,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_event(event: &str, payload: Value) -> EventFrame {
        EventFrame {
            event: event.into(),
            payload: Some(payload),
            seq: None,
        }
    }

    #[test]
    fn chat_delta_produces_text_event() {
        let mut state = TextFallbackState::new();
        state.reset_for_new_turn();

        let event = make_event(
            "chat",
            json!({ "state": "delta", "message": { "content": "Hello" }, "sessionKey": "sk-1" }),
        );
        let events = map_openclaw_event(&event, &mut state, Some("sk-1"));

        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentStreamEvent::Text(d) if d.content == "Hello"));
        assert_eq!(state.accumulated_text, "Hello");
    }

    #[test]
    fn chat_delta_v4_uses_delta_text() {
        let mut state = TextFallbackState::new();
        state.reset_for_new_turn();

        let e1 = make_event("chat", json!({ "state": "delta", "deltaText": "He" }));
        let e2 = make_event("chat", json!({ "state": "delta", "deltaText": "llo" }));

        let events1 = map_openclaw_event(&e1, &mut state, None);
        assert_eq!(events1.len(), 1);
        assert!(matches!(&events1[0], AgentStreamEvent::Text(d) if d.content == "He"));

        let events2 = map_openclaw_event(&e2, &mut state, None);
        assert_eq!(events2.len(), 1);
        assert!(matches!(&events2[0], AgentStreamEvent::Text(d) if d.content == "llo"));
        assert_eq!(state.accumulated_text, "Hello");
    }

    #[test]
    fn chat_delta_v4_replace_resets_buffer() {
        let mut state = TextFallbackState::new();
        state.reset_for_new_turn();
        state.accumulated_text = "fre".into();

        let event = make_event(
            "chat",
            json!({ "state": "delta", "deltaText": "fresh", "replace": true }),
        );
        let events = map_openclaw_event(&event, &mut state, None);

        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentStreamEvent::Text(d) if d.content == "sh"));
        assert_eq!(state.accumulated_text, "fresh");
    }

    #[test]
    fn chat_delta_v4_replace_rewrite_does_not_duplicate_prior_text() {
        let mut state = TextFallbackState::new();
        state.reset_for_new_turn();
        state.accumulated_text = "stale draft".into();

        let event = make_event(
            "chat",
            json!({ "state": "delta", "deltaText": "fresh", "replace": true }),
        );
        let events = map_openclaw_event(&event, &mut state, None);

        assert!(events.is_empty());
        assert_eq!(state.accumulated_text, "fresh");
    }

    #[test]
    fn chat_delta_computes_incremental() {
        let mut state = TextFallbackState::new();
        state.reset_for_new_turn();

        let e1 = make_event("chat", json!({ "state": "delta", "message": { "content": "He" } }));
        let e2 = make_event("chat", json!({ "state": "delta", "message": { "content": "Hello" } }));

        let events1 = map_openclaw_event(&e1, &mut state, None);
        assert_eq!(events1.len(), 1);
        assert!(matches!(&events1[0], AgentStreamEvent::Text(d) if d.content == "He"));

        let events2 = map_openclaw_event(&e2, &mut state, None);
        assert_eq!(events2.len(), 1);
        assert!(matches!(&events2[0], AgentStreamEvent::Text(d) if d.content == "llo"));
    }

    #[test]
    fn cumulative_text_rewrite_is_unicode_safe_and_not_duplicated() {
        let mut state = TextFallbackState::new();
        state.reset_for_new_turn();
        state.accumulated_text = "你好".into();

        let event = make_event(
            "chat",
            json!({ "state": "delta", "message": { "content": "你好吗" } }),
        );
        let events = map_openclaw_event(&event, &mut state, None);
        assert!(matches!(&events[0], AgentStreamEvent::Text(d) if d.content == "吗"));

        let rewrite = make_event(
            "chat",
            json!({ "state": "delta", "message": { "content": "您好" } }),
        );
        let events = map_openclaw_event(&rewrite, &mut state, None);
        assert!(events.is_empty());
        assert_eq!(state.accumulated_text, "您好");
    }

    #[test]
    fn chat_final_produces_finish() {
        let mut state = TextFallbackState::new();
        state.reset_for_new_turn();

        let event = make_event("chat", json!({ "state": "final", "sessionKey": "sk-1" }));
        let events = map_openclaw_event(&event, &mut state, None);

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentStreamEvent::Finish(d) if d.stop_reason == Some(crate::protocol::events::TurnStopReason::EndTurn)
        ));
        assert!(!state.turn_active);
    }

    #[test]
    fn chat_final_uses_layer2_fallback() {
        let mut state = TextFallbackState::new();
        state.reset_for_new_turn();
        state.agent_assistant_fallback = "Fallback text".into();

        let event = make_event("chat", json!({ "state": "final" }));
        let events = map_openclaw_event(&event, &mut state, None);

        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], AgentStreamEvent::Text(d) if d.content == "Fallback text"));
        assert!(matches!(&events[1], AgentStreamEvent::Finish(_)));
    }

    #[test]
    fn chat_error_produces_error_event() {
        let mut state = TextFallbackState::new();
        state.reset_for_new_turn();

        let event = make_event("chat", json!({ "state": "error", "errorMessage": "rate limit" }));
        let events = map_openclaw_event(&event, &mut state, None);

        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentStreamEvent::Error(d) if d.message == "rate limit"));
    }

    #[test]
    fn chat_error_preserves_assistant_error_message_content() {
        let mut state = TextFallbackState::new();
        state.reset_for_new_turn();

        let event = make_event(
            "chat",
            json!({
                "state": "error",
                "errorMessage": "provider unavailable",
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "Configure a provider first." }]
                }
            }),
        );
        let events = map_openclaw_event(&event, &mut state, None);

        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], AgentStreamEvent::Text(d) if d.content == "Configure a provider first."));
        assert!(matches!(&events[1], AgentStreamEvent::Error(d) if d.message == "provider unavailable"));
    }

    #[test]
    fn chat_aborted_produces_finish() {
        let mut state = TextFallbackState::new();
        state.reset_for_new_turn();

        let event = make_event("chat", json!({ "state": "aborted" }));
        let events = map_openclaw_event(&event, &mut state, None);

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentStreamEvent::Finish(d) if d.stop_reason == Some(crate::protocol::events::TurnStopReason::Cancelled)
        ));
    }

    #[test]
    fn agent_thinking_produces_thinking_event() {
        let mut state = TextFallbackState::new();
        let event = make_event(
            "agent.event",
            json!({ "stream": "thinking", "data": { "text": "Analyzing..." } }),
        );
        let events = map_openclaw_event(&event, &mut state, None);

        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentStreamEvent::Thinking(d) if d.content == "Analyzing..."));
    }

    #[test]
    fn agent_tool_start_produces_running() {
        let mut state = TextFallbackState::new();
        let event = make_event(
            "agent.event",
            json!({
                "stream": "tool",
                "data": {
                    "phase": "start",
                    "toolCallId": "tc-1",
                    "name": "read_file",
                    "args": { "path": "/tmp/test" }
                }
            }),
        );
        let events = map_openclaw_event(&event, &mut state, None);

        assert_eq!(events.len(), 1);
        if let AgentStreamEvent::ToolCall(tc) = &events[0] {
            assert_eq!(tc.call_id, "tc-1");
            assert_eq!(tc.name, "read_file");
            assert_eq!(tc.status, ToolCallStatus::Running);
        } else {
            panic!("Expected ToolCall");
        }
    }

    #[test]
    fn agent_event_filters_current_gateway_session_key_in_data() {
        let mut state = TextFallbackState::new();
        let event = make_event(
            "agent",
            json!({
                "runId": "run-1",
                "seq": 0,
                "stream": "thinking",
                "ts": 1,
                "data": {
                    "sessionKey": "other-session",
                    "text": "must not leak"
                }
            }),
        );
        let events = map_openclaw_event(&event, &mut state, Some("our-session"));
        assert!(events.is_empty());
    }

    #[test]
    fn agent_tool_result_produces_completed() {
        let mut state = TextFallbackState::new();
        let event = make_event(
            "agent.event",
            json!({
                "stream": "tool",
                "data": { "phase": "result", "toolCallId": "tc-1", "name": "read_file" }
            }),
        );
        let events = map_openclaw_event(&event, &mut state, None);

        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentStreamEvent::ToolCall(tc) if tc.status == ToolCallStatus::Completed));
    }

    #[test]
    fn agent_tool_error_produces_error_status() {
        let mut state = TextFallbackState::new();
        let event = make_event(
            "agent.event",
            json!({
                "stream": "tool",
                "data": { "phase": "result", "isError": true, "toolCallId": "tc-1", "name": "bash" }
            }),
        );
        let events = map_openclaw_event(&event, &mut state, None);

        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentStreamEvent::ToolCall(tc) if tc.status == ToolCallStatus::Error));
    }

    #[test]
    fn agent_assistant_buffers_for_fallback() {
        let mut state = TextFallbackState::new();
        state.reset_for_new_turn();

        let event = make_event(
            "agent.event",
            json!({ "stream": "assistant", "data": { "text": "buffered" } }),
        );
        let events = map_openclaw_event(&event, &mut state, None);

        assert!(events.is_empty());
        assert_eq!(state.agent_assistant_fallback, "buffered");
    }

    #[test]
    fn approval_request_produces_permission() {
        let mut state = TextFallbackState::new();
        let event = make_event(
            "exec.approval.requested",
            json!({
                "id": "req-1",
                "request": {
                    "command": "git status",
                    "commandPreview": "git status",
                    "host": "gateway",
                    "sessionKey": "sk-1",
                    "allowedDecisions": ["allow-once", "deny"]
                },
                "createdAtMs": 1,
                "expiresAtMs": 2
            }),
        );
        let events = map_openclaw_event(&event, &mut state, Some("sk-1"));

        assert_eq!(events.len(), 1);
        if let AgentStreamEvent::AcpPermission(AcpPermissionEventData::Confirmation(conf)) = &events[0] {
            assert_eq!(conf.call_id, "req-1");
            assert_eq!(conf.action, Some("git status".to_owned()));
            assert_eq!(conf.id, "req-1");
            assert_eq!(conf.options.len(), 2);
        } else {
            panic!("Expected AcpPermission(Confirmation)");
        }
    }

    #[test]
    fn approval_request_skips_other_session() {
        let mut state = TextFallbackState::new();
        let event = make_event(
            "exec.approval.requested",
            json!({
                "id": "req-other",
                "command": "rm -rf build",
                "sessionKey": "other-session",
                "allowedDecisions": ["allow-once", "deny"]
            }),
        );
        let events = map_openclaw_event(&event, &mut state, Some("our-session"));
        assert!(events.is_empty());
    }

    #[test]
    fn session_filtering_skips_other_sessions() {
        let mut state = TextFallbackState::new();
        state.reset_for_new_turn();

        let event = make_event(
            "chat",
            json!({ "state": "delta", "sessionKey": "other-session", "message": { "content": "x" } }),
        );
        let events = map_openclaw_event(&event, &mut state, Some("my-session"));

        assert!(events.is_empty());
    }

    #[test]
    fn session_filtering_passes_matching_session() {
        let mut state = TextFallbackState::new();
        state.reset_for_new_turn();

        let event = make_event(
            "chat",
            json!({ "state": "delta", "sessionKey": "my-session", "message": { "content": "x" } }),
        );
        let events = map_openclaw_event(&event, &mut state, Some("my-session"));

        assert!(!events.is_empty());
    }

    #[test]
    fn session_filtering_passes_when_no_session_key() {
        let mut state = TextFallbackState::new();
        state.reset_for_new_turn();

        let event = make_event("chat", json!({ "state": "delta", "message": { "content": "x" } }));
        let events = map_openclaw_event(&event, &mut state, Some("my-session"));

        assert!(!events.is_empty());
    }

    #[test]
    fn late_terminal_event_from_previous_run_is_ignored() {
        let mut state = TextFallbackState::new();
        state.reset_for_new_turn();

        let current = make_event(
            "chat",
            json!({
                "runId": "run-current",
                "sessionKey": "session-1",
                "seq": 0,
                "state": "delta",
                "deltaText": "new"
            }),
        );
        assert!(!map_openclaw_event(&current, &mut state, Some("session-1")).is_empty());

        let stale = make_event(
            "chat",
            json!({
                "runId": "run-old",
                "sessionKey": "session-1",
                "seq": 99,
                "state": "aborted"
            }),
        );
        assert!(map_openclaw_event(&stale, &mut state, Some("session-1")).is_empty());
        assert!(state.turn_active);
    }

    #[test]
    fn extract_text_string_content() {
        let msg = json!({ "content": "hello world" });
        assert_eq!(extract_text_from_message(&msg), Some("hello world".into()));
    }

    #[test]
    fn extract_text_block_content() {
        let msg = json!({ "content": [
            { "type": "text", "text": "part1" },
            { "type": "image", "url": "..." },
            { "type": "text", "text": "part2" }
        ]});
        assert_eq!(extract_text_from_message(&msg), Some("part1part2".into()));
    }

    #[test]
    fn extract_text_from_text_field() {
        let msg = json!({ "text": "fallback text" });
        assert_eq!(extract_text_from_message(&msg), Some("fallback text".into()));
    }

    #[test]
    fn extract_text_returns_none_for_empty() {
        let msg = json!({});
        assert_eq!(extract_text_from_message(&msg), None);
    }

    #[test]
    fn tick_and_health_events_ignored() {
        let mut state = TextFallbackState::new();
        let tick = EventFrame {
            event: "tick".into(),
            payload: Some(json!({ "ts": 12345 })),
            seq: None,
        };
        assert!(map_openclaw_event(&tick, &mut state, None).is_empty());

        let health = EventFrame {
            event: "health".into(),
            payload: None,
            seq: None,
        };
        assert!(map_openclaw_event(&health, &mut state, None).is_empty());
    }

    #[test]
    fn first_delta_auto_starts_turn() {
        let mut state = TextFallbackState::new();
        assert!(!state.turn_active);

        let event = make_event("chat", json!({ "state": "delta", "message": { "content": "Hi" } }));
        let events = map_openclaw_event(&event, &mut state, None);

        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], AgentStreamEvent::Start(_)));
        assert!(matches!(&events[1], AgentStreamEvent::Text(_)));
        assert!(state.turn_active);
    }
}

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use nomi_agent::output::OutputSink;
use tokio::sync::broadcast;

use crate::protocol::events::{
    AgentStatusEventData, AgentStreamEvent, ErrorEventData, FinishEventData, PlanEventData,
    StartEventData, TextEventData, ThinkingEventData, TipType, TipsEventData, ToolCallEventData,
    ToolCallStatus,
};

pub struct BackendOutputSink {
    event_tx: broadcast::Sender<AgentStreamEvent>,
    /// File-based memory directory for citation reflow. `None` = this session
    /// does not participate (companion sessions, or no base dir).
    distill_dir: Option<PathBuf>,
    /// Accumulates this turn's assistant text so the `<nomi-mem-citation>`
    /// block can be parsed at stream end. Reset on each stream start.
    turn_text: Mutex<String>,
    /// Tool calls that have been announced to the frontend but have not yet
    /// produced a tool result. Used to resolve partial provider tool-call
    /// streams before an automatic continuation pass.
    active_tool_calls: Mutex<HashMap<String, ActiveToolCall>>,
}

#[derive(Debug, Clone)]
struct ActiveToolCall {
    call_id: String,
    name: String,
    args: serde_json::Value,
    input: Option<serde_json::Value>,
}

/// Parse the `update_plan` tool result content into frontend plan entries.
/// The content may carry a soft-warning prefix, so we start from the first '{'.
fn parse_plan_entries(content: &str) -> Option<Vec<serde_json::Value>> {
    let start = content.find('{')?;
    let v: serde_json::Value = serde_json::from_str(&content[start..]).ok()?;
    if v.get("kind").and_then(|k| k.as_str()) != Some("plan_update") {
        return None;
    }
    let entries = v.get("entries")?.as_array()?.clone();
    Some(entries)
}

impl BackendOutputSink {
    pub fn new(event_tx: broadcast::Sender<AgentStreamEvent>) -> Self {
        Self {
            event_tx,
            distill_dir: None,
            turn_text: Mutex::new(String::new()),
            active_tool_calls: Mutex::new(HashMap::new()),
        }
    }

    /// Set the file-based memory directory used for citation reflow. `None`
    /// (the default) disables reflow for this session.
    pub fn with_distill_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.distill_dir = dir;
        self
    }

    fn internal_call_id(tool_use_id: &str) -> Option<String> {
        let id = tool_use_id.trim();
        if id.is_empty() {
            None
        } else {
            Some(format!("nomi-{id}"))
        }
    }

    fn remember_active_tool_call(
        &self,
        call_id: String,
        name: String,
        args: serde_json::Value,
        input: Option<serde_json::Value>,
    ) {
        match self.active_tool_calls.lock() {
            Ok(mut active) => {
                active.insert(
                    call_id.clone(),
                    ActiveToolCall {
                        call_id,
                        name,
                        args,
                        input,
                    },
                );
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "Failed to record active tool call for continuation cleanup"
                );
            }
        }
    }

    fn forget_active_tool_call(&self, call_id: &str) {
        match self.active_tool_calls.lock() {
            Ok(mut active) => {
                active.remove(call_id);
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "Failed to clear active tool call after tool result"
                );
            }
        }
    }

    pub(crate) fn complete_active_tool_calls_for_auto_continue(&self, reason: &str) {
        let interrupted: Vec<ActiveToolCall> = match self.active_tool_calls.lock() {
            Ok(mut active) => active.drain().map(|(_, data)| data).collect(),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "Failed to resolve active tool calls before automatic continuation"
                );
                Vec::new()
            }
        };

        if interrupted.is_empty() {
            return;
        }

        let output = format!(
            "Automatic continuation recovered from {reason}; this partial tool call was skipped and the task is continuing in smaller steps."
        );
        for active in interrupted {
            let _ = self.event_tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
                call_id: active.call_id,
                name: active.name,
                args: active.args,
                status: ToolCallStatus::Completed,
                input: active.input,
                output: Some(output.clone()),
                description: Some("Automatic continuation".to_owned()),
            }));
        }
    }

    /// Citation reflow: parse the `<nomi-mem-citation>` block from the turn's
    /// final assistant text and bump each cited memory file's usage stats.
    /// Silent on every failure — a stale citation or unreadable file must
    /// never disrupt the turn.
    fn reflow_citations(&self, full_text: &str) {
        let Some(dir) = self.distill_dir.as_ref() else {
            return;
        };
        let now = chrono::Utc::now();
        for fname in nomi_memory::distill::parse_citation_filenames(full_text) {
            if let Err(e) = nomi_memory::store::bump_memory_usage(dir, &fname, now) {
                tracing::debug!(file = %fname, error = %e, "citation reflow bump failed");
            }
        }
    }
}

impl OutputSink for BackendOutputSink {
    fn emit_text_delta(&self, text: &str, _msg_id: &str) {
        // Accumulate for end-of-turn citation reflow (only when participating).
        if self.distill_dir.is_some()
            && let Ok(mut buf) = self.turn_text.lock()
        {
            buf.push_str(text);
        }
        let _ = self.event_tx.send(AgentStreamEvent::Text(TextEventData {
            content: text.to_owned(),
        }));
    }

    fn emit_thinking(&self, text: &str, _msg_id: &str) {
        let _ = self.event_tx.send(AgentStreamEvent::Thinking(ThinkingEventData {
            content: text.to_owned(),
            subject: None,
            duration: None,
            status: None,
        }));
    }

    fn emit_tool_call_delta(&self, tool_use_id: &str, name: &str, input: Option<&str>) {
        let Some(call_id) = Self::internal_call_id(tool_use_id) else {
            tracing::error!(tool = name, "Cannot emit tool_call progress with empty tool_use_id");
            return;
        };

        let parsed_input: Option<serde_json::Value> =
            input.and_then(|value| serde_json::from_str(value).ok());

        tracing::info!(
            tool_use_id = %tool_use_id,
            call_id = %call_id,
            tool = name,
            has_input_preview = parsed_input.is_some(),
            status = ?ToolCallStatus::Running,
            "Emitting nomi tool_call progress event"
        );

        self.remember_active_tool_call(
            call_id.clone(),
            name.to_owned(),
            parsed_input.clone().unwrap_or(serde_json::Value::Null),
            parsed_input.clone(),
        );

        let _ = self.event_tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id,
            name: name.to_owned(),
            args: parsed_input.clone().unwrap_or(serde_json::Value::Null),
            status: ToolCallStatus::Running,
            input: parsed_input,
            output: None,
            description: None,
        }));
    }

    fn emit_model_activity(&self, _msg_id: &str, status: &str) {
        let _ = self
            .event_tx
            .send(AgentStreamEvent::AgentStatus(AgentStatusEventData {
                backend: "nomi".to_owned(),
                status: status.to_owned(),
                agent_name: Some("Nomi".to_owned()),
                session_id: None,
            }));
    }

    fn emit_tool_call(&self, tool_use_id: &str, name: &str, input: &str) {
        let Some(call_id) = Self::internal_call_id(tool_use_id) else {
            tracing::error!(tool = name, "Cannot emit tool_call with empty tool_use_id");
            return;
        };

        let parsed_input = serde_json::from_str(input).unwrap_or(serde_json::Value::String(input.to_owned()));

        tracing::debug!(
            tool_use_id = %tool_use_id,
            call_id = %call_id,
            tool = name,
            status = ?ToolCallStatus::Running,
            "Derived internal tool_call id from nomi tool_use_id"
        );
        tracing::info!(
            tool_use_id = %tool_use_id,
            call_id = %call_id,
            tool = name,
            status = ?ToolCallStatus::Running,
            "Emitting nomi tool_call event"
        );

        self.remember_active_tool_call(
            call_id.clone(),
            name.to_owned(),
            parsed_input.clone(),
            Some(parsed_input.clone()),
        );

        let _ = self.event_tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id,
            name: name.to_owned(),
            args: parsed_input.clone(),
            status: ToolCallStatus::Running,
            input: Some(parsed_input),
            output: None,
            description: None,
        }));
    }

    fn emit_tool_result(&self, tool_use_id: &str, name: &str, is_error: bool, content: &str) {
        // update_plan special case: emit a Plan event so the frontend renders
        // the checklist (MessagePlan) instead of a raw JSON tool card.
        if name == "update_plan" && !is_error {
            if let Some(entries) = parse_plan_entries(content) {
                let _ = self.event_tx.send(AgentStreamEvent::Plan(PlanEventData {
                    session_id: None,
                    entries,
                }));
                return;
            }
            // Unparsable -> fall through to a normal tool result (don't drop it).
        }

        let Some(call_id) = Self::internal_call_id(tool_use_id) else {
            tracing::error!(tool = name, "Cannot emit tool_result with empty tool_use_id");
            return;
        };

        self.forget_active_tool_call(&call_id);

        let status = if is_error {
            ToolCallStatus::Error
        } else {
            ToolCallStatus::Completed
        };

        tracing::info!(
            tool_use_id = %tool_use_id,
            call_id = %call_id,
            tool = name,
            status = ?status,
            "Emitting nomi tool_result event"
        );

        let _ = self.event_tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id,
            name: name.to_owned(),
            args: serde_json::Value::Null,
            status,
            input: None,
            output: if content.is_empty() {
                None
            } else {
                Some(content.to_owned())
            },
            description: None,
        }));
    }

    fn emit_stream_start(&self, _msg_id: &str) {
        // Reset the per-turn text buffer used for citation reflow.
        if let Ok(mut buf) = self.turn_text.lock() {
            buf.clear();
        }
        let _ = self
            .event_tx
            .send(AgentStreamEvent::Start(StartEventData { session_id: None }));
    }

    fn emit_stream_end(
        &self,
        _msg_id: &str,
        _turns: usize,
        _input_tokens: u64,
        _output_tokens: u64,
        _cache_creation_tokens: u64,
        _cache_read_tokens: u64,
    ) {
        // Citation reflow: parse the accumulated assistant text and bump the
        // cited memory files. Take the buffer so it doesn't linger.
        if self.distill_dir.is_some() {
            let full = self
                .turn_text
                .lock()
                .map(|mut b| std::mem::take(&mut *b))
                .unwrap_or_default();
            if !full.is_empty() {
                self.reflow_citations(&full);
            }
        }
        let _ = self
            .event_tx
            .send(AgentStreamEvent::Finish(FinishEventData {
                session_id: None,
                stop_reason: None,
            }));
    }

    fn emit_error(&self, msg: &str) {
        let _ = self
            .event_tx
            .send(AgentStreamEvent::Error(ErrorEventData::legacy(msg, None)));
    }

    fn emit_info(&self, msg: &str) {
        let _ = self.event_tx.send(AgentStreamEvent::Tips(TipsEventData {
            content: msg.to_owned(),
            tip_type: TipType::Success,
        }));
    }

    fn emit_warning(&self, msg: &str) {
        // Benign, non-fatal diagnostic: emit as Tips{Warning} on the broadcast —
        // NOT an Error — so the AutoWork / requirement orchestrator does not read
        // an otherwise-successful turn as failed. See OutputSink::emit_warning.
        let _ = self.event_tx.send(AgentStreamEvent::Tips(TipsEventData {
            content: msg.to_owned(),
            tip_type: TipType::Warning,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sink() -> (BackendOutputSink, broadcast::Receiver<AgentStreamEvent>) {
        let (tx, rx) = broadcast::channel(16);
        (BackendOutputSink::new(tx), rx)
    }

    #[test]
    fn emit_text_delta_sends_text_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_text_delta("hello", "msg-1");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Text(data) => assert_eq!(data.content, "hello"),
            other => panic!("Expected Text, got {:?}", other),
        }
    }

    #[test]
    fn emit_thinking_sends_thinking_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_thinking("analyzing...", "msg-1");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Thinking(data) => assert_eq!(data.content, "analyzing..."),
            other => panic!("Expected Thinking, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_call_sends_running_tool_call() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call("call_read_1", "Read", r#"{"path":"/tmp/a.txt"}"#);
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.name, "Read");
                assert_eq!(data.status, ToolCallStatus::Running);
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_call_delta_sends_running_tool_call_with_preview_input() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call_delta(
            "call_write_1",
            "Write",
            Some(r#"{"file_path":"/tmp/snake.html"}"#),
        );
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.call_id, "nomi-call_write_1");
                assert_eq!(data.name, "Write");
                assert_eq!(data.status, ToolCallStatus::Running);
                assert_eq!(data.input.as_ref().unwrap()["file_path"], "/tmp/snake.html");
                assert_eq!(data.args["file_path"], "/tmp/snake.html");
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn complete_active_tool_calls_for_auto_continue_marks_pending_tool_completed() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call_delta(
            "call_write_1",
            "Write",
            Some(r#"{"file_path":"/tmp/index.html"}"#),
        );
        let _running = rx.try_recv().unwrap();

        sink.complete_active_tool_calls_for_auto_continue("output token limit");

        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.call_id, "nomi-call_write_1");
                assert_eq!(data.name, "Write");
                assert_eq!(data.status, ToolCallStatus::Completed);
                assert_eq!(data.input.as_ref().unwrap()["file_path"], "/tmp/index.html");
                assert!(
                    data.output
                        .as_deref()
                        .unwrap()
                        .contains("Automatic continuation recovered from output token limit")
                );
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn complete_active_tool_calls_for_auto_continue_ignores_finished_tool() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call("call_read_1", "Read", r#"{"path":"/tmp/a.txt"}"#);
        let _running = rx.try_recv().unwrap();
        sink.emit_tool_result("call_read_1", "Read", false, "ok");
        let _completed = rx.try_recv().unwrap();

        sink.complete_active_tool_calls_for_auto_continue("output token limit");

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn emit_model_activity_sends_agent_status() {
        let (sink, mut rx) = make_sink();
        sink.emit_model_activity("msg-1", "preparing");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::AgentStatus(data) => {
                assert_eq!(data.backend, "nomi");
                assert_eq!(data.status, "preparing");
                assert_eq!(data.agent_name.as_deref(), Some("Nomi"));
            }
            other => panic!("Expected AgentStatus, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_result_success_sends_completed() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_result("call_read_1", "Read", false, "file content here");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.name, "Read");
                assert_eq!(data.status, ToolCallStatus::Completed);
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_result_error_sends_error_status() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_result("call_bash_1", "Bash", true, "command failed");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.name, "Bash");
                assert_eq!(data.status, ToolCallStatus::Error);
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn emit_warning_is_a_non_failing_tip_not_an_error_event() {
        // Benign, non-fatal diagnostics (autocompact failure, session save/index
        // hiccup, MCP-init failure, /compact failure) must reach the stream as a
        // non-failing Tips{Warning} — NOT an Error. The AutoWork / requirement
        // orchestrator classifies any non-retryable Error stream event as a FAILED
        // turn, so routing a benign warning through emit_error would re-pend the
        // requirement / burn an attempt / pause the tag on an otherwise-successful
        // turn (the regression this guards against).
        let (sink, mut rx) = make_sink();
        sink.emit_warning("Failed to save session: disk full");
        match rx.try_recv().expect("a warning event should be emitted") {
            AgentStreamEvent::Tips(data) => {
                assert_eq!(data.tip_type, TipType::Warning);
                assert!(data.content.contains("Failed to save session"));
            }
            other => panic!("emit_warning must be a non-failing Tips(Warning), got {:?}", other),
        }
    }

    #[test]
    fn duplicate_tool_names_use_distinct_internal_call_ids() {
        let (sink, mut rx) = make_sink();

        sink.emit_tool_call("call_a", "Glob", r#"{"pattern":"*.rs"}"#);
        sink.emit_tool_call("call_b", "Glob", r#"{"pattern":"*.toml"}"#);
        sink.emit_tool_result("call_a", "Glob", false, "first");
        sink.emit_tool_result("call_b", "Glob", false, "second");

        let events = (0..4).map(|_| rx.try_recv().unwrap()).collect::<Vec<_>>();

        let mut call_ids = vec![];
        for event in events {
            match event {
                AgentStreamEvent::ToolCall(data) => call_ids.push((data.call_id, data.status)),
                other => panic!("Expected ToolCall, got {:?}", other),
            }
        }

        assert_eq!(call_ids[0].0, "nomi-call_a");
        assert_eq!(call_ids[1].0, "nomi-call_b");
        assert_eq!(call_ids[2].0, "nomi-call_a");
        assert_eq!(call_ids[3].0, "nomi-call_b");
        assert_eq!(call_ids[2].1, ToolCallStatus::Completed);
        assert_eq!(call_ids[3].1, ToolCallStatus::Completed);
    }

    #[test]
    fn emit_stream_start_sends_start_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_stream_start("msg-1");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Start(_) => {}
            other => panic!("Expected Start, got {:?}", other),
        }
    }

    #[test]
    fn emit_stream_end_sends_finish_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_stream_end("msg-1", 3, 1000, 500, 100, 200);
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Finish(_) => {}
            other => panic!("Expected Finish, got {:?}", other),
        }
    }

    #[test]
    fn emit_error_sends_error_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_error("something went wrong");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Error(data) => assert_eq!(data.message, "something went wrong"),
            other => panic!("Expected Error, got {:?}", other),
        }
    }

    #[test]
    fn emit_info_sends_tips_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_info("operation completed");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Tips(data) => {
                assert_eq!(data.content, "operation completed");
                assert_eq!(data.tip_type, TipType::Success);
            }
            other => panic!("Expected Tips, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_call_carries_input() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call("call_glob_1", "Glob", r#"{"pattern":"**/*.rs"}"#);
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.name, "Glob");
                assert_eq!(data.status, ToolCallStatus::Running);
                assert!(data.input.is_some());
                assert_eq!(data.input.unwrap()["pattern"], "**/*.rs");
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_result_carries_output_and_matching_call_id() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call("call_glob_1", "Glob", r#"{"pattern":"**/*.rs"}"#);
        let start_event = rx.try_recv().unwrap();
        let start_call_id = match &start_event {
            AgentStreamEvent::ToolCall(data) => data.call_id.clone(),
            _ => panic!("Expected ToolCall"),
        };

        sink.emit_tool_result("call_glob_1", "Glob", false, "src/main.rs\nsrc/lib.rs");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.name, "Glob");
                assert_eq!(data.status, ToolCallStatus::Completed);
                assert_eq!(data.call_id, start_call_id);
                assert_eq!(data.output.as_deref(), Some("src/main.rs\nsrc/lib.rs"));
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_result_empty_content_omits_output() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_result("call_glob_1", "Glob", false, "");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert!(data.output.is_none());
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn no_panic_when_no_receivers() {
        let (tx, _) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx);
        sink.emit_text_delta("hello", "msg-1");
        sink.emit_thinking("thought", "msg-1");
        sink.emit_tool_call_delta("call_read_1", "Read", Some(r#"{"path":"/tmp/a.txt"}"#));
        sink.emit_tool_call("call_read_1", "Read", "{}");
        sink.emit_tool_result("call_read_1", "Read", false, "ok");
        sink.emit_stream_start("msg-1");
        sink.emit_stream_end("msg-1", 1, 100, 50, 0, 0);
        sink.emit_error("err");
        sink.emit_info("info");
    }

    #[test]
    fn update_plan_result_emits_plan_event() {
        let (sink, mut rx) = make_sink();
        let content = r#"{"kind":"plan_update","explanation":null,"entries":[{"content":"a","status":"completed"},{"content":"b","status":"in_progress"}]}"#;
        sink.emit_tool_result("call_1", "update_plan", false, content);
        match rx.try_recv().unwrap() {
            AgentStreamEvent::Plan(data) => {
                assert_eq!(data.entries.len(), 2);
                assert_eq!(data.entries[1]["status"], "in_progress");
            }
            other => panic!("expected Plan, got {other:?}"),
        }
        // No second ToolCall event (we returned early).
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn update_plan_with_warning_prefix_still_parses() {
        let (sink, mut rx) = make_sink();
        let content = "[note] 2 steps are in_progress; convention is exactly one. Plan rendered as submitted.\n{\"kind\":\"plan_update\",\"explanation\":null,\"entries\":[{\"content\":\"a\",\"status\":\"in_progress\"}]}";
        sink.emit_tool_result("call_1", "update_plan", false, content);
        match rx.try_recv().unwrap() {
            AgentStreamEvent::Plan(data) => assert_eq!(data.entries.len(), 1),
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    #[test]
    fn update_plan_unparsable_falls_through_to_toolcall() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_result("call_1", "update_plan", false, "not json");
        assert!(matches!(rx.try_recv().unwrap(), AgentStreamEvent::ToolCall(_)));
    }

    // -- citation reflow ------------------------------------------------------

    #[test]
    fn citation_reflow_bumps_cited_file_on_stream_end() {
        use nomi_memory::store::{read_memory, write_memory};
        use nomi_memory::types::{MemoryEntry, MemoryType};

        let tmp = tempfile::tempdir().unwrap();
        let entry = MemoryEntry::build("role", "user role", MemoryType::User, "senior dev");
        let path = write_memory(tmp.path(), &entry).unwrap();
        let filename = path.file_name().unwrap().to_str().unwrap().to_owned();

        let (tx, _rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx).with_distill_dir(Some(tmp.path().to_path_buf()));

        sink.emit_stream_start("m1");
        sink.emit_text_delta("Here is the answer.\n\n<nomi-mem-citation>\n", "m1");
        sink.emit_text_delta(&format!("{filename}|note=[used role]\n"), "m1");
        sink.emit_text_delta("</nomi-mem-citation>", "m1");
        sink.emit_stream_end("m1", 1, 10, 5, 0, 0);

        let read_back = read_memory(&path).unwrap();
        assert_eq!(read_back.frontmatter.usage_count, Some(1));
        assert!(read_back.frontmatter.last_used.is_some());
    }

    #[test]
    fn no_distill_dir_means_no_reflow_and_no_accumulation() {
        // Without a distill dir, the sink must not touch any file (and the
        // text buffer is never used).
        let (tx, _rx) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx); // distill_dir = None
        sink.emit_stream_start("m1");
        sink.emit_text_delta("<nomi-mem-citation>\nuser_role.md|note=[x]\n</nomi-mem-citation>", "m1");
        sink.emit_stream_end("m1", 1, 10, 5, 0, 0);
        // Nothing to assert beyond "did not panic / did not write" — the
        // turn_text buffer stays empty because distill_dir is None.
        assert!(sink.turn_text.lock().unwrap().is_empty());
    }
}

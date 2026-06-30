//! WorkerRunner（worker = 真实会话）: the scheduler primitive that runs ONE task
//! on a fresh nomi conversation, blocking until the turn completes or `timeout`,
//! and returns the agent's final assistant text.
//!
//! [`ConversationWorkerRunner`] is the production implementation. It **replicates
//! the gateway `nomi_agent_run` recipe** verbatim (see
//! `crates/backend/nomifun-gateway/src/caps_conversation.rs` —
//! `agent_run` / `await_turn` / `read_final_text` / `latest_assistant_text`):
//!
//! 1. Build a [`ProviderWithModel`] from the member (P1 supports Nomi-engine
//!    members only — a member without `provider_id` + `model` is rejected; non-Nomi
//!    / ACP members are out of P1 scope).
//! 2. Assemble the conversation `extra` (yolo + desktopGateway + orchestrator
//!    correlation ids + the supervisor brief as `system_prompt` + optional
//!    `workspace`).
//! 3. `conv.create(...)` a fresh nomi conversation, then `conv.send_message(...)`
//!    the task spec (origin `"orchestrator"`).
//! 4. `await_turn` (coarse 500ms poll until `!is_processing` or timeout) → settle
//!    (fine 25ms poll, 5s budget — avoids the reasoning-model `text:null` gotcha
//!    where the visible answer commits just after the turn releases) →
//!    `read_final_text` (newest `position == "left"` && `type == "text"` message).
//!
//! On timeout the runner still returns the `conversation_id` with `ok = false`
//! (the run keeps going; a caller could poll it later, mirroring
//! `nomi_agent_result`).
//!
//! [`MockWorkerRunner`] returns a fixed [`WorkerOutcome`] and is reused by the Run
//! engine (Task 6) to drive the scheduler without a live agent.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nomifun_ai_agent::IWorkerTaskManager;
use nomifun_api_types::{
    CreateConversationRequest, FleetMember, ListMessagesQuery, SendMessageRequest,
};
use nomifun_common::{AgentType, AppError, ProviderWithModel};
use nomifun_conversation::ConversationService;
use serde_json::{Value, json};

/// Outcome of running one task on a worker conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerOutcome {
    /// The conversation the task ran on (always present, even on timeout).
    pub conversation_id: i64,
    /// The agent's final assistant text, if any was produced.
    pub text: Option<String>,
    /// Whether the run produced a final text (false on timeout / no reply).
    pub ok: bool,
    /// Total tokens (`input + output`, summed across the turns this worker
    /// conversation ran) reported by the agent's `TurnCompleted` metrics and
    /// accumulated in the conversation runtime state. `None` when no usage was
    /// observed — a non-nomi member, a turn that never completed, or a runner
    /// (the mock) with no live token source. Written to `orch_run_tasks.tokens`
    /// by the engine's `settle_task_outcome` for the DAG/inspector per-node
    /// token display. NEVER fabricated: it is the real provider count or `None`.
    pub tokens: Option<i64>,
}

/// Runs a single task on a worker conversation, blocking until completion or
/// `timeout`, returning the final assistant text.
#[async_trait]
pub trait WorkerRunner: Send + Sync {
    /// 在一条新 worker 会话上执行一个任务,阻塞至完成或超时,返回最终文本。
    ///
    /// - `member` — the fleet member to run as (its provider/model).
    /// - `workspace_dir` — optional working directory for the conversation.
    /// - `run_id` / `task_id` — correlation ids stamped into `extra`.
    /// - `brief` — the supervisor brief, injected as the conversation `system_prompt`.
    /// - `task_spec` — the actual instruction sent as the first user message.
    /// - `timeout` — max wall-clock budget for the turn.
    /// - `on_started` — fired EXACTLY ONCE with the worker `conversation_id`, right
    ///   after the conversation is created and BEFORE send/await. Lets the engine
    ///   record the in-flight conversation (for cancellation) and immediately stamp
    ///   `task.conversation_id` (for the frontend live transcript).
    async fn run(
        &self,
        member: &FleetMember,
        workspace_dir: Option<&str>,
        run_id: &str,
        task_id: &str,
        brief: &str,
        task_spec: &str,
        timeout: Duration,
        on_started: Box<dyn FnOnce(i64) + Send>,
    ) -> Result<WorkerOutcome, AppError>;

    /// Read the worker conversation's CURRENT final assistant text, if any.
    ///
    /// Used by the **"采用为该节点产出" (adopt task result)** path
    /// ([`RunService::adopt_task_result`](crate::run_service::RunService::adopt_task_result)):
    /// after a user keeps chatting with a failed/stuck worker in the conversation
    /// content area, this pulls the worker's latest output back into the
    /// orchestration node on demand. The default returns `None` (test/mock runners
    /// have no live conversation store); the production
    /// [`ConversationWorkerRunner`] overrides it to read the conversation's latest
    /// assistant message via its inherent `read_final_text`.
    async fn read_final_output(&self, _conversation_id: &str) -> Option<String> {
        None
    }
}

/// Production [`WorkerRunner`]: runs the task on a real nomi conversation via
/// [`ConversationService`], replicating the gateway `nomi_agent_run` recipe.
pub struct ConversationWorkerRunner {
    conv: ConversationService,
    task_manager: Arc<dyn IWorkerTaskManager>,
    user_id: String,
}

impl ConversationWorkerRunner {
    pub fn new(conv: ConversationService, task_manager: Arc<dyn IWorkerTaskManager>, user_id: String) -> Self {
        Self {
            conv,
            task_manager,
            user_id,
        }
    }
}

#[async_trait]
impl WorkerRunner for ConversationWorkerRunner {
    /// Production override of [`WorkerRunner::read_final_output`]: delegate to the
    /// inherent `read_final_text` (the same helper the engine uses to settle a
    /// finished worker turn), so an adopted node carries the worker conversation's
    /// real latest assistant text.
    async fn read_final_output(&self, conversation_id: &str) -> Option<String> {
        self.read_final_text(conversation_id).await
    }

    async fn run(
        &self,
        member: &FleetMember,
        workspace_dir: Option<&str>,
        run_id: &str,
        task_id: &str,
        brief: &str,
        task_spec: &str,
        timeout: Duration,
        on_started: Box<dyn FnOnce(i64) + Send>,
    ) -> Result<WorkerOutcome, AppError> {
        // P1 supports Nomi-engine members: both provider_id and model are required.
        // Non-Nomi / ACP members (which carry their model differently) are out of
        // P1 scope — reject loudly rather than silently producing an empty pwm.
        let (Some(provider_id), Some(model)) = (member.provider_id.clone(), member.model.clone()) else {
            return Err(AppError::BadRequest(
                "worker member needs provider+model (P1 supports Nomi-engine members only)".to_owned(),
            ));
        };
        let pwm = ProviderWithModel {
            provider_id,
            model: model.clone(),
            use_model: Some(model),
        };

        let extra = build_worker_extra(
            run_id,
            task_id,
            brief,
            workspace_dir,
            member.system_prompt.as_deref(),
            &member.enabled_skills,
            &member.disabled_builtin_skills,
        );

        // Create the worker conversation. yolo: unattended orchestrator runs have
        // no approval UI; desktopGateway: full platform tool set. We call create()
        // directly (not via the HTTP route), so the extra keys are honored.
        let conv = self
            .conv
            .create(
                &self.user_id,
                CreateConversationRequest {
                    r#type: AgentType::Nomi,
                    name: Some(format!("Run {run_id} · {task_id}")),
                    model: Some(pwm),
                    source: None,
                    channel_chat_id: None,
                    extra,
                },
            )
            .await?;
        let id = conv.id.to_string();

        // Report the freshly-created conversation id BEFORE send/await, so the
        // engine can record the in-flight conversation (for cancellation) and
        // stamp task.conversation_id for the live transcript while the turn runs.
        on_started(conv.id);

        // Send the task spec as the first user message. The turn is claimed
        // synchronously before send_message returns, so await_turn sees
        // is_processing == true on its first poll.
        self.conv
            .send_message(
                &self.user_id,
                &id,
                SendMessageRequest {
                    content: task_spec.to_owned(),
                    files: vec![],
                    inject_skills: vec![],
                    hidden: false,
                    origin: Some("orchestrator".to_owned()),
                    channel_platform: None,
                },
                &self.task_manager,
            )
            .await?;

        // Coarse poll until the turn finishes or the budget elapses.
        let finished = self.await_turn(&id, timeout, Duration::from_millis(500)).await;
        if !finished {
            // Still running after timeout: hand back the conversation id with ok=false.
            // Take any token usage that DID accumulate (a long multi-turn run may
            // have completed earlier turns before the budget elapsed) — real count
            // or None, never fabricated.
            return Ok(WorkerOutcome {
                conversation_id: conv.id,
                text: None,
                ok: false,
                tokens: self.conv.take_turn_tokens(&id),
            });
        }

        // Settle: the terminal turn release can fire a few ms before the final
        // assistant `text` message commits (a reasoning model persists its visible
        // answer LAST). Mirror nomi_agent_result — gate on the runtime turn having
        // fully released before reading. An already-settled turn returns at once.
        let _ = self
            .await_turn(&id, Duration::from_secs(5), Duration::from_millis(25))
            .await;
        let text = self.read_final_text(&id).await;
        // Read (and remove) the conversation's accumulated token total. The relay
        // records the `TurnCompleted` usage BEFORE the turn claim releases, and we
        // only reach here AFTER `await_turn` observed the claim released, so the
        // total is fully written by now (no race). `None` when no usage event was
        // seen (the existing zero-source behaviour — task.tokens stays None).
        let tokens = self.conv.take_turn_tokens(&id);
        Ok(WorkerOutcome {
            conversation_id: conv.id,
            ok: text.is_some(),
            text,
            tokens,
        })
    }
}

impl ConversationWorkerRunner {
    /// Await an agent turn to completion (or until `timeout`), polling every
    /// `poll`. Returns true if the turn finished, false on timeout. An
    /// already-finished turn returns immediately (first check before any sleep).
    /// Copied from the gateway `await_turn` helper (deps → self.conv).
    async fn await_turn(&self, conv_id: &str, timeout: Duration, poll: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let summary = self.conv.runtime_summary_for(conv_id).await;
            if !summary.is_processing {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// Read the final assistant text of a (finished) conversation, if any.
    /// Copied from the gateway `read_final_text` helper (deps → self.conv).
    async fn read_final_text(&self, conv_id: &str) -> Option<String> {
        let messages = self
            .conv
            .list_messages(
                &self.user_id,
                conv_id,
                ListMessagesQuery {
                    page: Some(1),
                    page_size: Some(10),
                    order: Some("desc".to_owned()),
                    content_mode: None,
                    cursor: None,
                },
            )
            .await
            .ok()?;
        let v = serde_json::to_value(&messages).ok()?;
        latest_assistant_text(&v)
    }
}

/// Assemble the worker conversation `extra`: yolo + desktopGateway + orchestrator
/// correlation ids + the supervisor brief as `system_prompt`, plus an optional
/// `workspace`. Split out as a free function so it is unit-testable without a
/// live ConversationService.
///
/// **Persona inheritance (P4 Task 3, Change 1):** when the member is
/// assistant-backed it carries the assistant's persona/rule text in
/// `system_prompt` (Task 2 snapshot). We set it as `extra.preset_rules` — the
/// nomi factory (`factory/nomi.rs`) merges `preset_rules` AFTER `system_prompt`,
/// yielding `brief\n\npersona`, so the supervisor brief leads and the assistant
/// persona follows. We deliberately do NOT overwrite `extra.system_prompt`
/// (that is the brief). A blank/whitespace-only persona is dropped (no key).
///
/// **Skills inheritance (P4 Task 3, Change 2):** the worker calls
/// `ConversationService::create` directly (line above), so the create handler's
/// skill machinery runs on this `extra`: it consumes the request-only
/// `preset_enabled_skills` (assistant's enabled skills) and
/// `exclude_auto_inject_skills` (assistant's disabled builtins), computes the
/// initial `skills` snapshot via `compute_initial_skills`, and freezes it into
/// `extra.skills`. So we just forward the assistant's two skill lists here as
/// the canonical request-only keys; the existing handler does the rest (no
/// handler/factory changes). Empty lists are emitted as empty arrays — harmless
/// (the create handler treats them as "no preset / no exclusion").
fn build_worker_extra(
    run_id: &str,
    task_id: &str,
    brief: &str,
    workspace_dir: Option<&str>,
    persona: Option<&str>,
    enabled_skills: &[String],
    disabled_builtin_skills: &[String],
) -> Value {
    let mut extra = json!({
        "session_mode": "yolo",
        "desktopGateway": true,
        "orchestrator_run_id": run_id,
        "orchestrator_task_id": task_id,
        "system_prompt": brief,
        // Request-only skill-shaping inputs consumed by ConversationService::create:
        // preset_enabled_skills ∪ (auto_inject − exclude_auto_inject_skills) → extra.skills.
        // The assistant's enabled/disabled-builtin snapshot rides through verbatim.
        "preset_enabled_skills": enabled_skills,
        "exclude_auto_inject_skills": disabled_builtin_skills,
    });
    // Persona: assistant rule text appended after the brief by the nomi factory.
    if let Some(persona) = persona.map(str::trim).filter(|s| !s.is_empty()) {
        extra["preset_rules"] = json!(persona);
    }
    if let Some(ws) = workspace_dir.map(str::trim).filter(|s| !s.is_empty()) {
        extra["workspace"] = json!(ws);
    }
    extra
}

/// Walk a serialized message list (desc-ordered) and return the newest assistant
/// reply text — the first object with `position == "left"` and `type == "text"`,
/// whose `content` is shaped `{"content": "<text>"}`. Copied verbatim from the
/// gateway `latest_assistant_text` helper.
fn latest_assistant_text(v: &Value) -> Option<String> {
    match v {
        Value::Array(arr) => arr.iter().find_map(latest_assistant_text),
        Value::Object(map) => {
            let is_assistant_text = map.get("position").and_then(Value::as_str) == Some("left")
                && map.get("type").and_then(Value::as_str) == Some("text");
            if is_assistant_text
                && let Some(text) = map.get("content").and_then(|c| c.get("content")).and_then(Value::as_str)
            {
                return Some(text.to_owned());
            }
            map.values().find_map(latest_assistant_text)
        }
        _ => None,
    }
}

/// Fixed-outcome [`WorkerRunner`] for tests — returns the configured
/// [`WorkerOutcome`] regardless of inputs. Reused by the Run engine (Task 6) to
/// drive the scheduler without a live agent. Public (not `#[cfg(test)]`) so other
/// crates' / modules' tests can construct it.
pub struct MockWorkerRunner {
    pub outcome: WorkerOutcome,
    /// Artificial delay awaited (after firing `on_started`) before returning the
    /// outcome. Defaults to [`Duration::ZERO`]; Task 2's concurrency test sets a
    /// non-zero delay to create overlap windows between parallel workers.
    pub delay: Duration,
}

impl MockWorkerRunner {
    /// Build a mock that always succeeds with `text` on a fixed `conversation_id`,
    /// returning immediately (zero delay).
    pub fn with_text(conversation_id: i64, text: impl Into<String>) -> Self {
        Self::with_text_and_delay(conversation_id, text, Duration::ZERO)
    }

    /// Like [`with_text`](Self::with_text) but awaits `delay` (after `on_started`)
    /// before returning — used to overlap parallel workers in scheduler tests.
    pub fn with_text_and_delay(conversation_id: i64, text: impl Into<String>, delay: Duration) -> Self {
        let text = text.into();
        Self {
            outcome: WorkerOutcome {
                conversation_id,
                text: Some(text),
                ok: true,
                // The mock has no live token source — mirrors the production
                // zero-source path (task.tokens stays None). A test that needs a
                // token value sets `outcome.tokens` directly after construction.
                tokens: None,
            },
            delay,
        }
    }
}

#[async_trait]
impl WorkerRunner for MockWorkerRunner {
    async fn run(
        &self,
        _member: &FleetMember,
        _workspace_dir: Option<&str>,
        _run_id: &str,
        _task_id: &str,
        _brief: &str,
        _task_spec: &str,
        _timeout: Duration,
        on_started: Box<dyn FnOnce(i64) + Send>,
    ) -> Result<WorkerOutcome, AppError> {
        // Mirror the production runner: report the conversation id up front,
        // before any (simulated) work.
        on_started(self.outcome.conversation_id);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        Ok(self.outcome.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_worker_runner_returns_fixed_outcome() {
        let runner: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(42, "done"));
        let member = sample_member(None, None);
        let outcome = runner
            .run(
                &member,
                None,
                "run_1",
                "task_1",
                "brief",
                "spec",
                Duration::from_secs(1),
                Box::new(|_| {}),
            )
            .await
            .expect("mock never errors");
        assert_eq!(outcome.conversation_id, 42);
        assert_eq!(outcome.text.as_deref(), Some("done"));
        assert!(outcome.ok);
    }

    #[tokio::test]
    async fn mock_worker_runner_reports_conv_id_via_on_started() {
        let runner = MockWorkerRunner::with_text(7, "done");
        let member = sample_member(None, None);
        let seen = Arc::new(std::sync::Mutex::new(Vec::<i64>::new()));
        let sink = seen.clone();
        let outcome = runner
            .run(
                &member,
                None,
                "run_1",
                "task_1",
                "brief",
                "spec",
                Duration::from_secs(1),
                Box::new(move |conv_id| sink.lock().unwrap().push(conv_id)),
            )
            .await
            .expect("mock never errors");
        // on_started fires exactly once with the fixed conversation id.
        assert_eq!(*seen.lock().unwrap(), vec![7]);
        assert_eq!(outcome.conversation_id, 7);
    }

    #[tokio::test]
    async fn mock_worker_runner_respects_delay() {
        // A non-zero delay must be observed before the outcome returns; the
        // concurrency test in Task 2 relies on this to create overlap windows.
        let runner = MockWorkerRunner::with_text_and_delay(9, "done", Duration::from_millis(60));
        let member = sample_member(None, None);
        let start = Instant::now();
        let outcome = runner
            .run(
                &member,
                None,
                "run_1",
                "task_1",
                "brief",
                "spec",
                Duration::from_secs(1),
                Box::new(|_| {}),
            )
            .await
            .expect("mock never errors");
        assert!(
            start.elapsed() >= Duration::from_millis(60),
            "delay was not awaited (elapsed {:?})",
            start.elapsed()
        );
        assert_eq!(outcome.conversation_id, 9);
    }

    #[tokio::test]
    async fn mock_worker_runner_default_delay_is_zero() {
        // with_text keeps a zero delay so existing callers are unaffected.
        let runner = MockWorkerRunner::with_text(1, "x");
        assert_eq!(runner.delay, Duration::ZERO);
    }

    #[test]
    fn build_worker_extra_carries_correlation_keys_and_brief() {
        let extra = build_worker_extra("run_abc", "task_xyz", "you are a worker", None, None, &[], &[]);
        assert_eq!(extra["session_mode"], "yolo");
        assert_eq!(extra["desktopGateway"], true);
        assert_eq!(extra["orchestrator_run_id"], "run_abc");
        assert_eq!(extra["orchestrator_task_id"], "task_xyz");
        assert_eq!(extra["system_prompt"], "you are a worker");
        // No workspace_dir → key absent (not null).
        assert!(extra.get("workspace").is_none());
        // No persona → preset_rules key absent (the brief is NOT a persona).
        assert!(extra.get("preset_rules").is_none());
    }

    #[test]
    fn build_worker_extra_includes_trimmed_workspace() {
        let extra = build_worker_extra("r", "t", "b", Some("  /tmp/ws  "), None, &[], &[]);
        assert_eq!(extra["workspace"], "/tmp/ws");
    }

    #[test]
    fn build_worker_extra_ignores_blank_workspace() {
        let extra = build_worker_extra("r", "t", "b", Some("   "), None, &[], &[]);
        assert!(extra.get("workspace").is_none());
    }

    // (Change 1) An assistant-backed member's persona is set as `extra.preset_rules`
    // (NOT system_prompt — that stays the brief). The nomi factory merges
    // preset_rules after system_prompt → `brief\n\npersona`.
    #[test]
    fn build_worker_extra_sets_persona_as_preset_rules_without_touching_brief() {
        let extra = build_worker_extra(
            "r",
            "t",
            "supervisor brief",
            None,
            Some("你是一名严谨的研究员，始终引用来源。"),
            &[],
            &[],
        );
        // Brief stays as the system_prompt; persona rides as preset_rules.
        assert_eq!(extra["system_prompt"], "supervisor brief");
        assert_eq!(extra["preset_rules"], "你是一名严谨的研究员，始终引用来源。");
    }

    // A blank/whitespace-only persona is dropped — no preset_rules key.
    #[test]
    fn build_worker_extra_drops_blank_persona() {
        let empty = build_worker_extra("r", "t", "b", None, Some(""), &[], &[]);
        assert!(empty.get("preset_rules").is_none());
        let blank = build_worker_extra("r", "t", "b", None, Some("   \n  "), &[], &[]);
        assert!(blank.get("preset_rules").is_none());
        // Persona is trimmed before being stored.
        let padded = build_worker_extra("r", "t", "b", None, Some("  persona  "), &[], &[]);
        assert_eq!(padded["preset_rules"], "persona");
    }

    // (Change 2) The assistant's enabled/disabled-builtin skill lists ride through
    // as the request-only keys that ConversationService::create consumes to freeze
    // the `extra.skills` snapshot (preset_enabled_skills ∪ (auto − exclude)).
    #[test]
    fn build_worker_extra_forwards_skill_lists_as_request_only_keys() {
        let enabled = vec!["web_search".to_string(), "code_run".to_string()];
        let disabled = vec!["browser".to_string()];
        let extra = build_worker_extra("r", "t", "b", None, None, &enabled, &disabled);
        assert_eq!(
            extra["preset_enabled_skills"],
            json!(["web_search", "code_run"]),
            "enabled_skills must surface as preset_enabled_skills for the create handler"
        );
        assert_eq!(
            extra["exclude_auto_inject_skills"],
            json!(["browser"]),
            "disabled_builtin_skills must surface as exclude_auto_inject_skills"
        );
    }

    // Empty skill lists still emit empty arrays — the create handler treats them
    // as "no preset / no exclusion" (no behavior change for bare members).
    #[test]
    fn build_worker_extra_emits_empty_skill_arrays_when_member_has_none() {
        let extra = build_worker_extra("r", "t", "b", None, None, &[], &[]);
        assert_eq!(extra["preset_enabled_skills"], json!([]));
        assert_eq!(extra["exclude_auto_inject_skills"], json!([]));
    }

    #[test]
    fn latest_assistant_text_extracts_newest_left_text_from_desc_list() {
        // Serialized desc-ordered message list: newest first. The runner reads the
        // first left/text reply. Earlier (in the list) right/text and thinking
        // entries must be skipped.
        let list = json!({
            "items": [
                { "position": "left", "type": "text", "content": { "content": "final answer" } },
                { "position": "left", "type": "thinking", "content": { "content": "ignored reasoning" } },
                { "position": "right", "type": "text", "content": { "content": "the user prompt" } }
            ],
            "total": 3,
            "has_more": false
        });
        assert_eq!(latest_assistant_text(&list).as_deref(), Some("final answer"));
    }

    #[test]
    fn latest_assistant_text_skips_non_text_left_messages() {
        // A left/tool_call before the left/text must not be mistaken for the reply.
        let list = json!({
            "items": [
                { "position": "left", "type": "tool_call", "content": { "content": "{}" } },
                { "position": "left", "type": "text", "content": { "content": "real reply" } }
            ]
        });
        assert_eq!(latest_assistant_text(&list).as_deref(), Some("real reply"));
    }

    #[test]
    fn latest_assistant_text_none_when_only_user_messages() {
        let list = json!({
            "items": [
                { "position": "right", "type": "text", "content": { "content": "hi" } }
            ]
        });
        assert!(latest_assistant_text(&list).is_none());
    }

    fn sample_member(provider_id: Option<&str>, model: Option<&str>) -> FleetMember {
        FleetMember {
            id: "fm_1".to_owned(),
            agent_id: "agent_x".to_owned(),
            provider_id: provider_id.map(str::to_owned),
            model: model.map(str::to_owned),
            role_hint: None,
            capability_profile: None,
            constraints: None,
            sort_order: 0,
            description: None,
            system_prompt: None,
            enabled_skills: Vec::new(),
            disabled_builtin_skills: Vec::new(),
        }
    }
}

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

    /// Whether the worker conversation's MOST RECENT terminal turn ended on a
    /// RETRYABLE provider error (rate limit / timeout / transient gateway error —
    /// the persisted error message's `error.retryable` flag). The engine consults
    /// this when a worker turn produced NO final text, to decide whether to
    /// auto-retry the node (a transient blip self-heals) or fail it permanently.
    ///
    /// Default `false` so test/mock runners (no live conversation store) and any
    /// unknown error never loop — only an error EXPLICITLY marked retryable opts a
    /// node into the bounded auto-retry. The production [`ConversationWorkerRunner`]
    /// overrides it to read the conversation's latest error marker.
    async fn last_error_retryable(&self, _conversation_id: &str) -> bool {
        false
    }

    /// Whether the worker conversation carries ANY `content.type == "error"` marker.
    /// Distinguishes a NonRetryable provider error (marker present, `retryable=false`)
    /// from a NoMarker timeout / empty reply (no marker at all — a turn that returned
    /// `ok:false` without ever persisting a provider-error message). The engine's
    /// three-way classification retries a NoMarker failure under the SEPARATE
    /// `max_timeout_retries` budget, while a marked non-retryable error still fails
    /// fast. Default `false` (no marker) so test/mock runners with no live
    /// conversation store report NoMarker unless they explicitly override; the
    /// production [`ConversationWorkerRunner`] overrides it to read the marker.
    async fn last_error_present(&self, _conversation_id: &str) -> bool {
        false
    }

    /// A short human-readable failure reason (`<code>: <message>`) from the
    /// conversation's latest error marker, PERSISTED onto a permanently-failed task
    /// so the lead-report / escalation / diagnostic tools can show WHY it failed
    /// without re-reading the conversation. Default `None` (mock runners / no live
    /// conversation store); the production [`ConversationWorkerRunner`] overrides it.
    async fn last_error_summary(&self, _conversation_id: &str) -> Option<String> {
        None
    }

    /// 带角色的执行入口：默认忽略 `role` 委托给 [`Self::run`]（既有 mock/测试
    /// runner 零改动）。生产 [`ConversationWorkerRunner`] 覆写本方法，把受限
    /// 角色（searcher/reviewer/verifier）映射为 per-node 工具白名单 + 网关收缩。
    /// 引擎 dispatch 统一走本方法并传 `task.role` 与 `delegation_depth`（受控嵌套：
    /// 从 task.pattern_config 读出、烙进 worker 会话 extra）。
    #[allow(clippy::too_many_arguments)]
    async fn run_restricted(
        &self,
        role: Option<&str>,
        delegation_depth: u32,
        member: &FleetMember,
        workspace_dir: Option<&str>,
        run_id: &str,
        task_id: &str,
        brief: &str,
        task_spec: &str,
        timeout: Duration,
        on_started: Box<dyn FnOnce(i64) + Send>,
    ) -> Result<WorkerOutcome, AppError> {
        let _ = (role, delegation_depth);
        self.run(member, workspace_dir, run_id, task_id, brief, task_spec, timeout, on_started)
            .await
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

    /// Production override of [`WorkerRunner::last_error_retryable`]: scan the
    /// worker conversation's most recent messages for the newest provider-error
    /// marker and return its `error.retryable` flag (false when none / absent).
    async fn last_error_retryable(&self, conversation_id: &str) -> bool {
        self.read_latest_error_retryable(conversation_id).await
    }

    /// Production override of [`WorkerRunner::last_error_present`]: scan the worker
    /// conversation's most recent messages for ANY provider-error marker
    /// (`content.type == "error"`) and report whether one exists (regardless of its
    /// `retryable` flag). Lets the engine tell a marked failure (present) apart from
    /// a plain timeout / empty reply (absent).
    async fn last_error_present(&self, conversation_id: &str) -> bool {
        self.read_latest_error_present(conversation_id).await
    }

    async fn last_error_summary(&self, conversation_id: &str) -> Option<String> {
        self.read_latest_error_summary(conversation_id).await
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
        // 无角色入口：委托给带角色版本（生产逻辑在 run_restricted 里）。root 深度 0：
        // 无角色直入的 worker 视作委派链起点，其派生子任务将得深度 1。
        self.run_restricted(None, 0, member, workspace_dir, run_id, task_id, brief, task_spec, timeout, on_started)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_restricted(
        &self,
        role: Option<&str>,
        delegation_depth: u32,
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
            role,
            delegation_depth,
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

    /// Read the worker conversation's latest provider-error `retryable` flag.
    /// Lists the same recent (desc) window as [`read_final_text`](Self::read_final_text)
    /// and scans for the newest `content.type == "error"` marker's
    /// `error.retryable`; `false` on any list error / no error message / absent flag.
    async fn read_latest_error_retryable(&self, conv_id: &str) -> bool {
        let Ok(messages) = self
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
        else {
            return false;
        };
        match serde_json::to_value(&messages) {
            Ok(v) => latest_error_retryable(&v),
            Err(_) => false,
        }
    }

    /// Read whether the worker conversation's recent window carries ANY provider-
    /// error marker (`content.type == "error"`). Lists the same recent (desc) window
    /// as [`read_latest_error_retryable`](Self::read_latest_error_retryable) and
    /// mirrors its fetch path, but only tests for the marker's PRESENCE (not its
    /// `retryable` flag); `false` on any list error / no error marker.
    async fn read_latest_error_present(&self, conv_id: &str) -> bool {
        let Ok(messages) = self
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
        else {
            return false;
        };
        match serde_json::to_value(&messages) {
            Ok(v) => latest_error_present(&v),
            Err(_) => false,
        }
    }

    /// Read the conversation's latest error marker as a `<code>: <message>` string
    /// (best-effort → `None` on any read/parse failure or when no error marker
    /// exists). Mirrors [`Self::read_latest_error_retryable`]'s message-fetch path.
    async fn read_latest_error_summary(&self, conv_id: &str) -> Option<String> {
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
        latest_error_summary(&serde_json::to_value(&messages).ok()?)
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
#[allow(clippy::too_many_arguments)]
fn build_worker_extra(
    run_id: &str,
    task_id: &str,
    brief: &str,
    workspace_dir: Option<&str>,
    persona: Option<&str>,
    enabled_skills: &[String],
    disabled_builtin_skills: &[String],
    role: Option<&str>,
    // 受控嵌套深度（Phase 3b W7d）：本 worker 会话在委派链上的层级。root lead=0（其
    // 会话 extra 由别处构造、不含本键 → 读回 0）；lead 的直属 worker=1；被 worker 再
    // 委派出的子任务=2。网关两个追加口（nomi_run_add_tasks / nomi_spawn 收敛）据此
    // 钳制深度（DELEGATION_DEPTH_LIMIT），防无界自我繁殖。
    delegation_depth: u32,
) -> Value {
    // 受限角色（searcher/reviewer 只读、verifier 只读+Bash）：收缩引擎工具白名单，
    // 且不授桌面网关 —— 只读 worker 拿全量 nomi_* 桌面控制等于静默升权。
    // 其余（implementer / planner 的中文角色标签 / 无角色）保持现状全量能力。
    let restricted = role_allowed_tools(role);
    let mut extra = json!({
        "session_mode": "yolo",
        "desktopGateway": restricted.is_none(),
        "orchestrator_run_id": run_id,
        "orchestrator_task_id": task_id,
        // 受控嵌套：本 worker 的委派深度。网关追加口读取此键 +1 与 LIMIT 比较后钳制。
        "orchestrator_delegation_depth": delegation_depth,
        "system_prompt": brief,
        // Request-only skill-shaping inputs consumed by ConversationService::create:
        // preset_enabled_skills ∪ (auto_inject − exclude_auto_inject_skills) → extra.skills.
        // The assistant's enabled/disabled-builtin snapshot rides through verbatim.
        "preset_enabled_skills": enabled_skills,
        "exclude_auto_inject_skills": disabled_builtin_skills,
    });
    if let Some(tools) = restricted {
        extra["allowed_tools"] = json!(tools);
    }
    // Persona: assistant rule text appended after the brief by the nomi factory.
    if let Some(persona) = persona.map(str::trim).filter(|s| !s.is_empty()) {
        extra["preset_rules"] = json!(persona);
    }
    if let Some(ws) = workspace_dir.map(str::trim).filter(|s| !s.is_empty()) {
        extra["workspace"] = json!(ws);
    }
    extra
}

/// 受限角色 → 引擎工具白名单（与进程内 Spawn 的 role_tools 语义一致）。
/// `None` = 不限制（implementer / 未知 / planner 的中文角色标签 / 无角色）——
/// 只有显式的英文受限角色才收缩，规划器产出的中文 role 永不误伤。
fn role_allowed_tools(role: Option<&str>) -> Option<Vec<&'static str>> {
    match role.map(|r| r.to_ascii_lowercase()).as_deref() {
        Some("searcher" | "scout" | "reviewer") => Some(vec!["Read", "Grep", "Glob"]),
        Some("verifier" | "tester") => Some(vec!["Read", "Grep", "Glob", "Bash"]),
        _ => None,
    }
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

/// Scan a serialized message list (desc-ordered) for the newest PROVIDER-error
/// message and return its `error.retryable` flag. A provider error is a message
/// whose parsed `content.type == "error"` (carrying the persisted agent
/// [`ErrorInfo`](nomi_protocol-style) `{code, message, retryable}` under
/// `content.error`); the rate-limit case is `retryable: true`. A tool-call error
/// (which has `status:"error"` but no `content.type == "error"` marker) and any
/// non-error message are skipped — so this returns the classification of the turn-
/// ending provider error, not an intermediate tool failure. `false` when no
/// provider error is present or the flag is absent, so an unknown error never loops.
fn latest_error_retryable(v: &Value) -> bool {
    match v {
        Value::Array(arr) => arr.iter().find_map(error_retryable_flag).unwrap_or(false),
        _ => error_retryable_flag(v).unwrap_or(false),
    }
}

/// For a single serialized message, `Some(retryable)` iff its `content` marks a
/// provider error (`content.type == "error"`), else `None` — so a desc-ordered
/// `find_map` skips non-error messages and yields the NEWEST provider error's flag.
/// The flag is `content.error.retryable` (absent → `false`).
fn error_retryable_flag(v: &Value) -> Option<bool> {
    let content = v.as_object()?.get("content")?;
    if content.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    Some(
        content
            .get("error")
            .and_then(|e| e.get("retryable"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    )
}

/// Whether a serialized message list (array or single) carries ANY provider-error
/// marker (`content.type == "error"`). Mirrors [`latest_error_retryable`]'s
/// array/single walk but tests only for PRESENCE — used to tell a marked failure
/// (present) apart from a plain timeout / empty reply (absent).
fn latest_error_present(v: &Value) -> bool {
    match v {
        Value::Array(arr) => arr.iter().any(error_marker_present),
        _ => error_marker_present(v),
    }
}

/// True iff this single serialized message is a provider-error marker
/// (`content.type == "error"`).
fn error_marker_present(v: &Value) -> bool {
    v.as_object()
        .and_then(|o| o.get("content"))
        .and_then(|c| c.get("type"))
        .and_then(Value::as_str)
        == Some("error")
}

/// The latest error marker rendered as a short `<code>: <message>` reason (or one
/// of them if only one is present); `None` when no error marker exists. Mirrors
/// [`latest_error_retryable`]'s array/single walk.
fn latest_error_summary(v: &Value) -> Option<String> {
    match v {
        Value::Array(arr) => arr.iter().find_map(error_summary_str),
        _ => error_summary_str(v),
    }
}

fn error_summary_str(v: &Value) -> Option<String> {
    let content = v.as_object()?.get("content")?;
    if content.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let err = content.get("error")?;
    let code = err.get("code").and_then(Value::as_str);
    let message = err.get("message").and_then(Value::as_str);
    match (code, message) {
        (Some(c), Some(m)) => Some(format!("{c}: {m}")),
        (Some(c), None) => Some(c.to_string()),
        (None, Some(m)) => Some(m.to_string()),
        (None, None) => None,
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

    // 迁移 024: the retryable-error scanner the engine reads to decide auto-retry.
    #[test]
    fn latest_error_retryable_reads_provider_error_flag() {
        // Desc-ordered list (newest first); the rate-limit marker carries
        // content.type=="error" + error.retryable==true → retryable.
        let rate_limited = json!([
            {"position":"left","type":"tips","content":{
                "content":"rate limited","type":"error",
                "error":{"code":"USER_LLM_PROVIDER_RATE_LIMITED","retryable":true}}},
            {"position":"right","type":"text","content":{"content":"the task"}}
        ]);
        assert!(latest_error_retryable(&rate_limited), "rate-limit error is retryable");

        // A non-retryable provider error (auth) → false.
        let auth = json!([{"position":"left","type":"tips","content":{
            "content":"auth","type":"error",
            "error":{"code":"USER_LLM_PROVIDER_AUTH_FAILED","retryable":false}}}]);
        assert!(!latest_error_retryable(&auth), "auth error is NOT retryable");

        // No error message at all → false.
        let clean = json!([{"position":"left","type":"text","content":{"content":"hi"}}]);
        assert!(!latest_error_retryable(&clean), "no error → not retryable");

        // A tool-call error (status error but NO content.type=="error") is skipped;
        // the provider error below it (newest-first) decides.
        let tool_then_provider = json!([
            {"position":"left","type":"tool_call","content":{"name":"Bash","status":"error"}},
            {"position":"left","type":"tips","content":{
                "content":"rate","type":"error","error":{"retryable":true}}}
        ]);
        assert!(
            latest_error_retryable(&tool_then_provider),
            "tool error skipped; provider error's retryable flag is read"
        );

        // A provider error with no `retryable` field defaults to NOT retrying.
        let no_flag = json!([{"position":"left","type":"tips","content":{
            "content":"x","type":"error","error":{"code":"X"}}}]);
        assert!(!latest_error_retryable(&no_flag), "missing flag defaults to not-retryable");
    }

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
        let extra = build_worker_extra("run_abc", "task_xyz", "you are a worker", None, None, &[], &[], None, 0);
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

    // 受限角色 → 引擎工具白名单（与进程内 Spawn role_tools 语义一致）。
    #[test]
    fn role_allowed_tools_maps_restricted_roles() {
        assert_eq!(role_allowed_tools(Some("searcher")), Some(vec!["Read", "Grep", "Glob"]));
        assert_eq!(role_allowed_tools(Some("Reviewer")), Some(vec!["Read", "Grep", "Glob"])); // 大小写不敏感
        assert_eq!(role_allowed_tools(Some("verifier")), Some(vec!["Read", "Grep", "Glob", "Bash"]));
        // implementer / planner 的中文角色标签 / 无角色 → 不限制（绝不误伤）。
        assert_eq!(role_allowed_tools(Some("implementer")), None);
        assert_eq!(role_allowed_tools(Some("前端")), None);
        assert_eq!(role_allowed_tools(None), None);
    }

    // 受限角色：带 allowed_tools 白名单，且不授桌面网关（只读 worker 拿全量
    // nomi_* 桌面控制 = 静默升权）。无角色/implementer：现状完全不变。
    #[test]
    fn build_worker_extra_restricted_role_shrinks_tools_and_gateway() {
        let extra = build_worker_extra("run_abc", "task_xyz", "brief", None, None, &[], &[], Some("searcher"), 0);
        assert_eq!(extra["desktopGateway"], false, "受限角色不得静默升权到全量网关");
        assert_eq!(extra["allowed_tools"], json!(["Read", "Grep", "Glob"]));

        let verify = build_worker_extra("run_abc", "task_xyz", "brief", None, None, &[], &[], Some("verifier"), 0);
        assert_eq!(verify["allowed_tools"], json!(["Read", "Grep", "Glob", "Bash"]));

        let full = build_worker_extra("run_abc", "task_xyz", "brief", None, None, &[], &[], Some("implementer"), 0);
        assert_eq!(full["desktopGateway"], true);
        assert!(full.get("allowed_tools").is_none());
    }

    #[test]
    fn build_worker_extra_includes_trimmed_workspace() {
        let extra = build_worker_extra("r", "t", "b", Some("  /tmp/ws  "), None, &[], &[], None, 0);
        assert_eq!(extra["workspace"], "/tmp/ws");
    }

    #[test]
    fn build_worker_extra_ignores_blank_workspace() {
        let extra = build_worker_extra("r", "t", "b", Some("   "), None, &[], &[], None, 0);
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
            None,
            0,
        );
        // Brief stays as the system_prompt; persona rides as preset_rules.
        assert_eq!(extra["system_prompt"], "supervisor brief");
        assert_eq!(extra["preset_rules"], "你是一名严谨的研究员，始终引用来源。");
    }

    // A blank/whitespace-only persona is dropped — no preset_rules key.
    #[test]
    fn build_worker_extra_drops_blank_persona() {
        let empty = build_worker_extra("r", "t", "b", None, Some(""), &[], &[], None, 0);
        assert!(empty.get("preset_rules").is_none());
        let blank = build_worker_extra("r", "t", "b", None, Some("   \n  "), &[], &[], None, 0);
        assert!(blank.get("preset_rules").is_none());
        // Persona is trimmed before being stored.
        let padded = build_worker_extra("r", "t", "b", None, Some("  persona  "), &[], &[], None, 0);
        assert_eq!(padded["preset_rules"], "persona");
    }

    // (Change 2) The assistant's enabled/disabled-builtin skill lists ride through
    // as the request-only keys that ConversationService::create consumes to freeze
    // the `extra.skills` snapshot (preset_enabled_skills ∪ (auto − exclude)).
    #[test]
    fn build_worker_extra_forwards_skill_lists_as_request_only_keys() {
        let enabled = vec!["web_search".to_string(), "code_run".to_string()];
        let disabled = vec!["browser".to_string()];
        let extra = build_worker_extra("r", "t", "b", None, None, &enabled, &disabled, None, 0);
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
        let extra = build_worker_extra("r", "t", "b", None, None, &[], &[], None, 0);
        assert_eq!(extra["preset_enabled_skills"], json!([]));
        assert_eq!(extra["exclude_auto_inject_skills"], json!([]));
    }

    // 受控嵌套（Phase 3b W7d）：build_worker_extra 把委派深度烙进 worker 会话 extra
    // 的 orchestrator_delegation_depth 键（网关追加口据此 +1 与 LIMIT 比较后钳制）。
    #[test]
    fn build_worker_extra_stamps_delegation_depth() {
        let root = build_worker_extra("r", "t", "b", None, None, &[], &[], None, 0);
        assert_eq!(root["orchestrator_delegation_depth"], 0);
        let deep = build_worker_extra("r", "t", "b", None, None, &[], &[], Some("implementer"), 2);
        assert_eq!(deep["orchestrator_delegation_depth"], 2);
        // 受限角色也照常烙深度（键与角色收缩正交）。
        let restricted = build_worker_extra("r", "t", "b", None, None, &[], &[], Some("searcher"), 1);
        assert_eq!(restricted["orchestrator_delegation_depth"], 1);
        assert_eq!(restricted["desktopGateway"], false);
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

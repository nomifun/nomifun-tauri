//! Adapter from one durable [`ExecutionAttempt`](nomifun_api_types::ExecutionAttempt)
//! to one real Agent conversation.
//!
//! This module deliberately knows nothing about planning, DAG scheduling or
//! execution lifecycle. It creates a conversation, requires the caller to
//! persist the attempt's `ConversationExecutionLink`, executes one turn, and returns the
//! observed output. The scheduler is therefore able to cancel an attempt as
//! soon as the conversation exists, without a correlation-id race.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nomifun_ai_agent::{
    AgentRuntimeRegistry,
    artifact_store::{ArtifactStore, PersistedArtifact},
};
use nomifun_api_types::{
    CreateConversationRequest, ExecutionModelPool, ExecutionModelRef, ExecutionParticipant,
    ListMessagesQuery, MessageResponse, SendMessageRequest,
};
use nomifun_common::{
    AgentToolPolicy, AgentType, AppError, DecisionPolicy, DelegationPolicy,
    MAX_AGENT_DELEGATION_DEPTH, MessagePosition, MessageStatus, MessageType, ProviderId,
    ProviderWithModel,
};
use nomifun_conversation::{AgentExecutionConversationPort, ConversationService};
use nomifun_db::AgentExecutionTurnAuthority;
use serde_json::{Value, json};

const ARTIFACT_RECEIPT_PAGE_SIZE: u32 = 100;
// Keep receipt consumption aligned with ArtifactStore::verify_existing_path.
// The store repeats this limit against real metadata before reading bytes, so
// a forged small receipt cannot make us hash an arbitrarily large file.
const MAX_VERIFIED_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

/// Async callback invoked immediately after the Agent conversation is created
/// and before its first message is sent. The scheduler uses it to persist the
/// attempt link and make cancellation/recovery race-free.
pub(crate) type AttemptStarted = Box<
    dyn FnOnce(
            String,
        ) -> Pin<Box<dyn Future<Output = Result<AgentExecutionTurnAuthority, AppError>> + Send>>
        + Send,
>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttemptOutcome {
    pub conversation_id: String,
    pub text: Option<String>,
    /// Verified artifact paths observed in this attempt's current turn.
    pub output_files: Vec<String>,
    pub ok: bool,
    pub tokens: Option<i64>,
}

#[async_trait]
pub(crate) trait AttemptRunner: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        owner_id: &str,
        participant: &ExecutionParticipant,
        execution_model_pool: &[ExecutionModelRef],
        workspace_dir: Option<&str>,
        step_title: &str,
        tool_policy: AgentToolPolicy,
        delegation_policy: DelegationPolicy,
        delegation_depth: i64,
        decision_policy: DecisionPolicy,
        attempt_creation_key: &str,
        brief: &str,
        step_spec: &str,
        timeout: Duration,
        on_started: AttemptStarted,
    ) -> Result<AttemptOutcome, AppError>;

    /// Continue a waiting attempt in its existing Agent conversation after a
    /// user decision. The same durable attempt and transcript remain attached.
    async fn continue_with_input(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _authority: AgentExecutionTurnAuthority,
        _input: &str,
        _timeout: Duration,
    ) -> Result<AttemptOutcome, AppError> {
        Err(AppError::BadRequest(
            "this attempt runner cannot continue an existing attempt".to_owned(),
        ))
    }

    /// Best-effort is insufficient here: a queued attempt recovered after a
    /// process crash must remove any creation-keyed conversation that never
    /// acquired its durable Execution link.  Implementations may no-op only
    /// when they cannot create external conversation state.
    async fn discard_unlinked_creation(
        &self,
        _owner_id: &str,
        _attempt_creation_key: &str,
    ) -> Result<(), AppError> {
        Ok(())
    }

    async fn read_final_output(&self, _owner_id: &str, _conversation_id: &str) -> Option<String> {
        None
    }

    async fn read_output_files(&self, _owner_id: &str, _conversation_id: &str) -> Vec<String> {
        Vec::new()
    }

    async fn last_error_retryable(&self, _owner_id: &str, _conversation_id: &str) -> bool {
        false
    }

    async fn last_error_present(&self, _owner_id: &str, _conversation_id: &str) -> bool {
        false
    }

    async fn last_error_summary(&self, _owner_id: &str, _conversation_id: &str) -> Option<String> {
        None
    }
}

/// Production adapter. `ConversationService` owns the real Agent runtime; this
/// type only performs the create/send/wait/read choreography for one attempt.
pub(crate) struct ConversationAttemptRunner {
    conv: ConversationService,
    execution_port: AgentExecutionConversationPort,
}

impl ConversationAttemptRunner {
    pub fn new(conv: ConversationService, runtime_registry: Arc<dyn AgentRuntimeRegistry>) -> Self {
        let execution_port = conv.agent_execution_port(runtime_registry);
        Self {
            conv,
            execution_port,
        }
    }

    async fn await_turn(&self, conversation_id: &str, timeout: Duration, poll: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if !self.conv.runtime_summary_for(conversation_id).await.is_processing {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(poll).await;
        }
    }

    async fn recent_messages(&self, owner_id: &str, conversation_id: &str) -> Option<Value> {
        let messages = self
            .conv
            .list_messages(
                owner_id,
                conversation_id,
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
        serde_json::to_value(messages).ok()
    }

    #[allow(clippy::too_many_arguments)]
    async fn deliver_turn(
        &self,
        owner_id: &str,
        conversation_id: &str,
        operation_id: &str,
        authority: AgentExecutionTurnAuthority,
        content: &str,
        origin: &str,
        timeout: Duration,
    ) -> Result<AttemptOutcome, AppError> {
        let delivery = self
            .execution_port
            .deliver_turn(
                owner_id,
                conversation_id,
                operation_id,
                authority,
                SendMessageRequest {
                    content: content.to_owned(),
                    files: vec![],
                    inject_skills: vec![],
                    hidden: false,
                    origin: Some(origin.to_owned()),
                    channel_platform: None,
                },
            )
            .await?;
        let boundary_message_id = delivery.message_id.clone();
        if delivery.completed {
            let projection = self
                .output_files_for_turn(owner_id, conversation_id, &boundary_message_id)
                .await;
            return Ok(AttemptOutcome {
                conversation_id: conversation_id.to_owned(),
                text: delivery.result_text,
                output_files: projection.files,
                ok: delivery.result_ok.unwrap_or(false) && projection.integrity_ok,
                tokens: self.conv.take_turn_tokens(conversation_id),
            });
        }
        if !self
            .await_turn(conversation_id, timeout, Duration::from_millis(500))
            .await
        {
            return Ok(AttemptOutcome {
                conversation_id: conversation_id.to_owned(),
                text: None,
                output_files: Vec::new(),
                ok: false,
                tokens: self.conv.take_turn_tokens(conversation_id),
            });
        }
        let _ = self
            .await_turn(
                conversation_id,
                Duration::from_secs(5),
                Duration::from_millis(25),
            )
            .await;
        if let Some(receipt) = self
            .execution_port
            .delivery_result(owner_id, conversation_id, operation_id)
            .await?
            .filter(|receipt| receipt.completed)
        {
            let projection = self
                .output_files_for_turn(owner_id, conversation_id, &receipt.message_id)
                .await;
            return Ok(AttemptOutcome {
                conversation_id: conversation_id.to_owned(),
                text: receipt.result_text,
                output_files: projection.files,
                ok: receipt.result_ok.unwrap_or(false) && projection.integrity_ok,
                tokens: self.conv.take_turn_tokens(conversation_id),
            });
        }
        // Runtime idleness is not a delivery receipt. Reading the newest
        // assistant row here could select a previous or concurrently delivered
        // turn and falsely complete this attempt. Without the exact operation
        // receipt above, fail closed and let the scheduler retry/report the
        // missing terminal delivery.
        tracing::warn!(
            conversation_id,
            operation_id,
            boundary_message_id,
            "agent turn became idle without a completed delivery receipt"
        );
        Ok(missing_delivery_receipt_outcome(
            conversation_id,
            self.conv.take_turn_tokens(conversation_id),
        ))
    }

    /// Project only artifact receipts belonging to the exact delivered turn.
    ///
    /// The durable delivery receipt identifies the exact right-side user row.
    /// Tool rows use a separate wire-turn id, stamped identically into their
    /// `msg_id` and `content.turn_id`. We page newest-first to that user-row
    /// boundary, reset at any intervening user turn, require the tool ids to be
    /// self-consistent, and fail closed unless the boundary is found.
    async fn output_files_for_turn(
        &self,
        owner_id: &str,
        conversation_id: &str,
        boundary_message_id: &str,
    ) -> ArtifactProjectionResult {
        self.output_files_from_projection(
            owner_id,
            conversation_id,
            TurnArtifactProjection::for_boundary(boundary_message_id),
        )
        .await
    }

    async fn latest_output_files(&self, owner_id: &str, conversation_id: &str) -> Vec<String> {
        self.output_files_from_projection(
            owner_id,
            conversation_id,
            TurnArtifactProjection::for_latest_turn(),
        )
        .await
        .files
    }

    async fn output_files_from_projection(
        &self,
        owner_id: &str,
        conversation_id: &str,
        mut projection: TurnArtifactProjection<'_>,
    ) -> ArtifactProjectionResult {
        let mut page = 1_u32;
        loop {
            let messages = match self
                .conv
                .list_messages(
                    owner_id,
                    conversation_id,
                    ListMessagesQuery {
                        page: Some(page),
                        page_size: Some(ARTIFACT_RECEIPT_PAGE_SIZE),
                        order: Some("desc".to_owned()),
                        content_mode: None,
                        cursor: None,
                    },
                )
                .await
            {
                Ok(messages) => messages,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        conversation_id,
                        requested_boundary = projection.boundary_label(),
                        "failed to page current-turn artifact receipts"
                    );
                    return ArtifactProjectionResult::failed();
                }
            };
            projection.ingest_page(&messages.items);
            if projection.boundary_seen() || !messages.has_more {
                break;
            }
            let Some(next_page) = page.checked_add(1) else {
                return ArtifactProjectionResult::failed();
            };
            page = next_page;
        }

        let workspace = self
            .conversation_workspace(owner_id, conversation_id)
            .await;
        projection.finish(workspace.as_deref())
    }

    async fn conversation_workspace(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Option<PathBuf> {
        let conversation = self.conv.get(owner_id, conversation_id).await.ok()?;
        let workspace = conversation
            .extra
            .get("workspace")
            .and_then(Value::as_str)?
            .trim();
        if workspace.is_empty() {
            return None;
        }
        let canonical = std::fs::canonicalize(workspace).ok()?;
        canonical.is_dir().then_some(canonical)
    }
}

#[async_trait]
impl AttemptRunner for ConversationAttemptRunner {
    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        owner_id: &str,
        participant: &ExecutionParticipant,
        execution_model_pool: &[ExecutionModelRef],
        workspace_dir: Option<&str>,
        step_title: &str,
        tool_policy: AgentToolPolicy,
        delegation_policy: DelegationPolicy,
        delegation_depth: i64,
        decision_policy: DecisionPolicy,
        attempt_creation_key: &str,
        brief: &str,
        step_spec: &str,
        timeout: Duration,
        on_started: AttemptStarted,
    ) -> Result<AttemptOutcome, AppError> {
        let (Some(provider_id), Some(model)) =
            (participant.provider_id.clone(), participant.model.clone())
        else {
            return Err(AppError::BadRequest(
                "execution participant needs a provider and model".to_owned(),
            ));
        };
        ProviderId::try_from(provider_id.as_str()).map_err(|_| {
            AppError::BadRequest(
                "execution participant has a non-canonical provider_id".to_owned(),
            )
        })?;
        if model.trim().is_empty() || model.trim() != model {
            return Err(AppError::BadRequest(
                "execution participant has an invalid model".to_owned(),
            ));
        }
        let provider = ProviderWithModel {
            provider_id,
            model: model.clone(),
            use_model: Some(model),
        };

        let mut extra = build_agent_extra(
            brief,
            workspace_dir,
            participant.system_prompt.as_deref(),
            &participant.enabled_skills,
            &participant.disabled_builtin_skills,
            tool_policy,
            delegation_depth >= MAX_AGENT_DELEGATION_DEPTH,
        );
        if let Some(snapshot) = participant.preset_snapshot.as_ref() {
            extra["preset_id"] = Value::String(snapshot.preset_id.clone());
            extra["preset_revision"] = Value::Number(snapshot.preset_revision.into());
            extra["preset_snapshot"] = serde_json::to_value(snapshot)
                .map_err(|error| AppError::Internal(format!("encode preset snapshot: {error}")))?;
        }

        let request = CreateConversationRequest {
            r#type: AgentType::Nomi,
            name: Some(format!("协作 · {}", step_title.trim())),
            model: Some(provider),
            source: None,
            channel_chat_id: None,
            preset_id: None,
            preset_overrides: None,
            delegation_policy: if delegation_depth >= MAX_AGENT_DELEGATION_DEPTH {
                DelegationPolicy::Disabled
            } else {
                delegation_policy
            },
            execution_model_pool: Some(ExecutionModelPool::Range {
                models: execution_model_pool.to_vec(),
            }),
            decision_policy,
            execution_template_id: None,
            extra,
        };
        let created = if let Some(snapshot) = participant.preset_snapshot.clone() {
            self.conv
                .create_from_preset_snapshot_idempotent(
                    owner_id,
                    request,
                    snapshot,
                    attempt_creation_key,
                )
                .await
        } else {
            self.conv
                .create_idempotent(owner_id, request, attempt_creation_key)
                .await
        };
        let conversation = match created {
            Ok(conversation) => conversation,
            Err(error) => {
                if let Err(cleanup_error) = self
                    .conv
                    .discard_unlinked_creation(owner_id, attempt_creation_key)
                    .await
                {
                    tracing::warn!(%cleanup_error, "failed to discard partially-created attempt conversation");
                }
                return Err(error);
            }
        };

        // This callback is awaited before the Agent can start. An outbox/link
        // failure leaves no untracked in-flight turn.
        let authority = match on_started(conversation.conversation_id.clone()).await {
            Ok(authority) => authority,
            Err(error) => {
            // If the link commit succeeded but its acknowledgement was lost,
            // the Conversation deletion guard rejects this cleanup.  Otherwise
            // the creation key and row are removed together, leaving no orphan.
            match self
                .conv
                .discard_unlinked_creation(owner_id, attempt_creation_key)
                .await
            {
                Ok(()) => {}
                Err(AppError::Conflict(_)) => {}
                Err(cleanup_error) => {
                    tracing::warn!(%cleanup_error, "failed to discard unlinked attempt conversation");
                }
            }
            return Err(error);
            }
        };

        let operation_id = format!("{attempt_creation_key}:initial-turn");
        self.deliver_turn(
            owner_id,
            &conversation.conversation_id,
            &operation_id,
            authority,
            step_spec,
            "agent_execution",
            timeout,
        )
        .await
    }

    async fn continue_with_input(
        &self,
        owner_id: &str,
        conversation_id: &str,
        operation_id: &str,
        authority: AgentExecutionTurnAuthority,
        input: &str,
        timeout: Duration,
    ) -> Result<AttemptOutcome, AppError> {
        self.deliver_turn(
            owner_id,
            conversation_id,
            operation_id,
            authority,
            input,
            "agent_execution_decision",
            timeout,
        )
        .await
    }

    async fn discard_unlinked_creation(
        &self,
        owner_id: &str,
        attempt_creation_key: &str,
    ) -> Result<(), AppError> {
        self.conv
            .discard_unlinked_creation(owner_id, attempt_creation_key)
            .await
    }

    async fn read_final_output(&self, owner_id: &str, conversation_id: &str) -> Option<String> {
        self.recent_messages(owner_id, conversation_id)
            .await
            .as_ref()
            .and_then(latest_assistant_text)
    }

    async fn read_output_files(&self, owner_id: &str, conversation_id: &str) -> Vec<String> {
        // Adoption has no stored delivery id, but the latest canonical
        // right-side boundary is reliable: only its immediately preceding
        // newest-first segment is considered, never the whole conversation.
        self.latest_output_files(owner_id, conversation_id).await
    }

    async fn last_error_retryable(&self, owner_id: &str, conversation_id: &str) -> bool {
        self.recent_messages(owner_id, conversation_id)
            .await
            .as_ref()
            .is_some_and(latest_error_retryable)
    }

    async fn last_error_present(&self, owner_id: &str, conversation_id: &str) -> bool {
        self.recent_messages(owner_id, conversation_id)
            .await
            .as_ref()
            .is_some_and(latest_error_present)
    }

    async fn last_error_summary(&self, owner_id: &str, conversation_id: &str) -> Option<String> {
        self.recent_messages(owner_id, conversation_id)
            .await
            .as_ref()
            .and_then(latest_error_summary)
    }
}

/// Runtime configuration only. Execution/step/attempt identity is intentionally
/// absent: the durable `ConversationExecutionLink` is the sole relation source.
#[allow(clippy::too_many_arguments)]
fn build_agent_extra(
    brief: &str,
    workspace_dir: Option<&str>,
    persona: Option<&str>,
    enabled_skills: &[String],
    disabled_builtin_skills: &[String],
    tool_policy: AgentToolPolicy,
    exclude_delegation: bool,
) -> Value {
    let restricted = tool_policy_allowed_tools(tool_policy);
    let mut extra = json!({
        "session_mode": "yolo",
        "system_prompt": brief,
        "preset_enabled_skills": enabled_skills,
        "exclude_auto_inject_skills": disabled_builtin_skills,
    });
    if let Some(tools) = restricted {
        extra["allowed_tools"] = json!(tools);
    }
    if exclude_delegation {
        // Subtractive gateway projection: depth stays private in SQLite, while
        // the ceiling Attempt never receives nomi_delegate in MCP tools/list.
        extra["gateway_excluded_tools"] = json!(["nomi_delegate"]);
    }
    if let Some(persona) = persona.map(str::trim).filter(|value| !value.is_empty()) {
        extra["preset_rules"] = json!(persona);
    }
    if let Some(workspace) = workspace_dir.map(str::trim).filter(|value| !value.is_empty()) {
        extra["workspace"] = json!(workspace);
    }
    extra
}

fn tool_policy_allowed_tools(policy: AgentToolPolicy) -> Option<Vec<&'static str>> {
    match policy {
        AgentToolPolicy::Full => None,
        AgentToolPolicy::ReadOnly => Some(vec!["Read", "Grep", "Glob"]),
        AgentToolPolicy::ReadShell => Some(vec!["Read", "Grep", "Glob", "Bash"]),
    }
}

fn latest_assistant_text(value: &Value) -> Option<String> {
    match value {
        Value::Array(values) => values.iter().find_map(latest_assistant_text),
        Value::Object(map) => {
            let is_text = map.get("position").and_then(Value::as_str) == Some("left")
                && map.get("type").and_then(Value::as_str) == Some("text");
            if is_text
                && let Some(text) = map
                    .get("content")
                    .and_then(|content| content.get("content"))
                    .and_then(Value::as_str)
            {
                return Some(text.to_owned());
            }
            map.values().find_map(latest_assistant_text)
        }
        _ => None,
    }
}

/// Runtime idleness and transcript contents are observational only. The
/// operation-scoped durable receipt is the sole authority which may mark an
/// Agent turn successful, so its absence always produces a failed outcome.
fn missing_delivery_receipt_outcome(
    conversation_id: &str,
    tokens: Option<i64>,
) -> AttemptOutcome {
    AttemptOutcome {
        conversation_id: conversation_id.to_owned(),
        text: None,
        output_files: Vec::new(),
        ok: false,
        tokens,
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ArtifactProjectionResult {
    files: Vec<String>,
    integrity_ok: bool,
}

impl ArtifactProjectionResult {
    fn failed() -> Self {
        Self {
            files: Vec::new(),
            integrity_ok: false,
        }
    }
}

#[derive(Debug)]
struct TurnArtifactProjection<'a> {
    boundary_message_id: Option<&'a str>,
    boundary_seen: bool,
    receipts: Vec<PersistedArtifact>,
    invalid_artifact_claim: bool,
}

impl<'a> TurnArtifactProjection<'a> {
    fn for_boundary(boundary_message_id: &'a str) -> Self {
        Self {
            boundary_message_id: Some(boundary_message_id),
            boundary_seen: false,
            receipts: Vec::new(),
            invalid_artifact_claim: false,
        }
    }

    fn for_latest_turn() -> Self {
        Self {
            boundary_message_id: None,
            boundary_seen: false,
            receipts: Vec::new(),
            invalid_artifact_claim: false,
        }
    }

    fn ingest_page(&mut self, messages: &[MessageResponse]) {
        if self.boundary_seen {
            return;
        }
        for message in messages {
            if is_right_turn_boundary(message) {
                if self
                    .boundary_message_id
                    .map_or(true, |boundary| message.message_id == boundary)
                {
                    self.boundary_seen = true;
                    return;
                }
                // We crossed a more recent turn. Receipts collected above that
                // boundary belong to it, not to the requested delivery.
                self.receipts.clear();
                self.invalid_artifact_claim = false;
                continue;
            }
            let has_claim = message_has_artifact_claim(message);
            match completed_artifact_receipts(message) {
                Some(receipts) => self.receipts.extend(receipts),
                None if has_claim => self.invalid_artifact_claim = true,
                None => {}
            }
        }
    }

    fn boundary_seen(&self) -> bool {
        self.boundary_seen
    }

    fn boundary_label(&self) -> &str {
        self.boundary_message_id.unwrap_or("<latest>")
    }

    fn finish(self, workspace: Option<&Path>) -> ArtifactProjectionResult {
        if !self.boundary_seen {
            return ArtifactProjectionResult::failed();
        }
        if self.receipts.is_empty() {
            return ArtifactProjectionResult {
                files: Vec::new(),
                integrity_ok: !self.invalid_artifact_claim,
            };
        }
        let Some(workspace) = workspace else {
            return ArtifactProjectionResult::failed();
        };
        let mut files = BTreeSet::new();
        let mut integrity_ok = !self.invalid_artifact_claim;
        for receipt in &self.receipts {
            match verify_artifact_receipt(workspace, receipt) {
                Some(path) => {
                    files.insert(path);
                }
                None => integrity_ok = false,
            }
        }
        ArtifactProjectionResult {
            files: files.into_iter().collect(),
            integrity_ok,
        }
    }
}

fn is_right_turn_boundary(message: &MessageResponse) -> bool {
    message.msg_id.as_deref() == Some(message.message_id.as_str())
        && message.r#type == MessageType::Text
        && message.position == Some(MessagePosition::Right)
        && message.status == Some(MessageStatus::Finish)
}

fn message_has_artifact_claim(message: &MessageResponse) -> bool {
    match message.r#type {
        MessageType::ToolCall => message
            .content
            .get("artifacts")
            .is_some_and(|artifacts| !artifacts.as_array().is_some_and(Vec::is_empty)),
        MessageType::AcpToolCall => message
            .content
            .get("update")
            .and_then(|update| update.get("content"))
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("artifact" | "artifact_error")
                    )
                })
            }),
        _ => false,
    }
}

fn completed_artifact_receipts(message: &MessageResponse) -> Option<Vec<PersistedArtifact>> {
    let wire_turn_id = message.msg_id.as_deref()?;
    if wire_turn_id.trim().is_empty()
        || message.status != Some(MessageStatus::Finish)
        || message.content.get("turn_id").and_then(Value::as_str) != Some(wire_turn_id)
        || message
            .content
            .get("artifact_delivery_committed")
            .and_then(Value::as_bool)
            != Some(true)
    {
        return None;
    }

    match message.r#type {
        MessageType::ToolCall => {
            if message.content.get("status").and_then(Value::as_str) != Some("completed") {
                return None;
            }
            let artifacts = message.content.get("artifacts")?.as_array()?;
            artifacts
                .iter()
                .cloned()
                .map(serde_json::from_value::<PersistedArtifact>)
                .collect::<Result<Vec<_>, _>>()
                .ok()
        }
        MessageType::AcpToolCall => {
            let update = message.content.get("update")?.as_object()?;
            if update.get("status").and_then(Value::as_str) != Some("completed") {
                return None;
            }
            let items = update.get("content")?.as_array()?;
            let mut artifacts = Vec::new();
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("artifact") => {
                        let artifact = item.get("artifact")?.clone();
                        artifacts.push(serde_json::from_value::<PersistedArtifact>(artifact).ok()?);
                    }
                    // A completed update carrying an artifact failure is not a
                    // trustworthy terminal receipt, even if another item looks valid.
                    Some("artifact_error") => return None,
                    _ => {}
                }
            }
            Some(artifacts)
        }
        _ => None,
    }
}

fn verify_artifact_receipt(workspace: &Path, artifact: &PersistedArtifact) -> Option<String> {
    if artifact.id.trim().is_empty()
        || artifact.mime_type.trim().is_empty()
        || artifact.path.trim().is_empty()
        || artifact.relative_path.trim().is_empty()
        || artifact.size_bytes == 0
        || artifact.size_bytes > MAX_VERIFIED_ARTIFACT_BYTES
        || artifact.sha256.len() != 64
        || !artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }

    if !Path::new(&artifact.path).is_absolute() {
        return None;
    }
    portable_relative_path(&artifact.relative_path)?;

    // Reuse the authoritative delivery verifier: canonical workspace
    // containment, regular/non-empty file checks, the 512 MiB metadata cap,
    // complete format validation, and SHA-256 are all repeated here.
    let verified = ArtifactStore::new(workspace)
        .verify_existing_path(&artifact.path)
        .ok()?;
    if verified.kind != artifact.kind
        || verified.mime_type != artifact.mime_type
        || verified.relative_path != artifact.relative_path
        || verified.size_bytes != artifact.size_bytes
        || !verified.sha256.eq_ignore_ascii_case(&artifact.sha256)
    {
        return None;
    }

    Some(verified.path)
}

fn portable_relative_path(value: &str) -> Option<PathBuf> {
    let mut path = PathBuf::new();
    for segment in value.split('/') {
        if segment.is_empty()
            || matches!(segment, "." | "..")
            || segment.contains(['\\', ':', '\0'])
        {
            return None;
        }
        path.push(segment);
    }
    (!path.as_os_str().is_empty() && !path.is_absolute()).then_some(path)
}

fn latest_error_retryable(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().find_map(error_retryable_flag).unwrap_or(false),
        _ => error_retryable_flag(value).unwrap_or(false),
    }
}

fn error_retryable_flag(value: &Value) -> Option<bool> {
    let content = value.as_object()?.get("content")?;
    if content.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    Some(
        content
            .get("error")
            .and_then(|error| error.get("retryable"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    )
}

fn latest_error_present(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(error_marker_present),
        _ => error_marker_present(value),
    }
}

fn error_marker_present(value: &Value) -> bool {
    value
        .as_object()
        .and_then(|object| object.get("content"))
        .and_then(|content| content.get("type"))
        .and_then(Value::as_str)
        == Some("error")
}

fn latest_error_summary(value: &Value) -> Option<String> {
    match value {
        Value::Array(values) => values.iter().find_map(error_summary),
        _ => error_summary(value),
    }
}

fn error_summary(value: &Value) -> Option<String> {
    let content = value.as_object()?.get("content")?;
    if content.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let error = content.get("error")?;
    match (
        error.get("code").and_then(Value::as_str),
        error.get("message").and_then(Value::as_str),
    ) {
        (Some(code), Some(message)) => Some(format!("{code}: {message}")),
        (Some(code), None) => Some(code.to_owned()),
        (None, Some(message)) => Some(message.to_owned()),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::{TimestampMs, generate_id};
    use sha2::{Digest, Sha256};

    const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8000-000000000201";
    const CURRENT_WIRE_TURN_ID: &str = "0190f5fe-7c00-7a00-8000-000000000211";
    const CURRENT_USER_TURN_ID: &str = "0190f5fe-7c00-7a00-8000-000000000212";
    const NEWER_WIRE_TURN_ID: &str = "0190f5fe-7c00-7a00-8000-000000000213";
    const NEWER_USER_TURN_ID: &str = "0190f5fe-7c00-7a00-8000-000000000214";
    const OLDER_WIRE_TURN_ID: &str = "0190f5fe-7c00-7a00-8000-000000000215";
    const OLDER_USER_TURN_ID: &str = "0190f5fe-7c00-7a00-8000-000000000216";

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn message(
        id: &str,
        msg_id: &str,
        message_type: MessageType,
        position: MessagePosition,
        status: MessageStatus,
        content: Value,
    ) -> MessageResponse {
        MessageResponse {
            message_id: id.to_owned(),
            conversation_id: CONVERSATION_ID.to_owned(),
            msg_id: Some(msg_id.to_owned()),
            r#type: message_type,
            content,
            position: Some(position),
            status: Some(status),
            hidden: false,
            created_at: TimestampMs::from(1),
        }
    }

    fn boundary(id: &str) -> MessageResponse {
        message(
            id,
            id,
            MessageType::Text,
            MessagePosition::Right,
            MessageStatus::Finish,
            json!({"content":"generate the requested artifact"}),
        )
    }

    fn artifact_receipt(workspace: &Path, file_name: &str, bytes: &[u8]) -> Value {
        let path = workspace.join(file_name);
        std::fs::write(&path, bytes).unwrap();
        let canonical = std::fs::canonicalize(path).unwrap();
        json!({
            "id": generate_id(),
            "kind": "file",
            "mime_type": "application/octet-stream",
            "path": canonical.to_string_lossy(),
            "relative_path": file_name,
            "size_bytes": bytes.len(),
            "sha256": sha256_hex(bytes),
        })
    }

    fn completed_tool_message(call_id: &str, turn_id: &str, artifact: Value) -> MessageResponse {
        message(
            &generate_id(),
            turn_id,
            MessageType::ToolCall,
            MessagePosition::Left,
            MessageStatus::Finish,
            json!({
                "call_id": call_id,
                "name": "generate_file",
                "status": "completed",
                "turn_id": turn_id,
                "artifact_delivery_committed": true,
                "artifacts": [artifact],
            }),
        )
    }

    fn projected_result(
        workspace: &Path,
        boundary_id: &str,
        pages: &[Vec<MessageResponse>],
    ) -> ArtifactProjectionResult {
        let workspace = std::fs::canonicalize(workspace).unwrap();
        let mut projection = TurnArtifactProjection::for_boundary(boundary_id);
        for page in pages {
            projection.ingest_page(page);
        }
        projection.finish(Some(&workspace))
    }

    fn projected_paths(
        workspace: &Path,
        boundary_id: &str,
        pages: &[Vec<MessageResponse>],
    ) -> Vec<String> {
        projected_result(workspace, boundary_id, pages).files
    }

    fn projected_latest_paths(workspace: &Path, pages: &[Vec<MessageResponse>]) -> Vec<String> {
        let workspace = std::fs::canonicalize(workspace).unwrap();
        let mut projection = TurnArtifactProjection::for_latest_turn();
        for page in pages {
            projection.ingest_page(page);
        }
        projection.finish(Some(&workspace)).files
    }

    #[test]
    fn runtime_extra_has_no_execution_identity_cache() {
        let extra = build_agent_extra(
            "brief",
            None,
            None,
            &[],
            &[],
            AgentToolPolicy::Full,
            false,
        );
        assert!(extra.get("execution_id").is_none());
        assert!(extra.get("step_id").is_none());
        assert!(extra.get("attempt_id").is_none());
        assert!(extra.get("delegation_depth").is_none());
    }

    #[test]
    fn recursion_ceiling_removes_delegate_without_exposing_depth() {
        let extra = build_agent_extra(
            "brief",
            None,
            None,
            &[],
            &[],
            AgentToolPolicy::Full,
            true,
        );
        assert_eq!(extra["gateway_excluded_tools"], json!(["nomi_delegate"]));
        assert!(extra.get("delegation_depth").is_none());
    }

    #[test]
    fn explicit_tool_policy_is_the_only_runtime_tool_narrowing() {
        assert_eq!(
            tool_policy_allowed_tools(AgentToolPolicy::ReadOnly).unwrap(),
            ["Read", "Grep", "Glob"]
        );
        assert_eq!(
            tool_policy_allowed_tools(AgentToolPolicy::ReadShell).unwrap(),
            ["Read", "Grep", "Glob", "Bash"]
        );
        assert!(tool_policy_allowed_tools(AgentToolPolicy::Full).is_none());
    }

    #[test]
    fn idle_without_delivery_receipt_ignores_old_or_concurrent_assistant_text() {
        let unrelated_transcript = json!([
            {
                "type": "text",
                "position": "left",
                "content": {"content": "concurrent turn finished"}
            },
            {
                "type": "text",
                "position": "left",
                "content": {"content": "historical turn finished"}
            }
        ]);
        // This is exactly the transcript signal the legacy fallback trusted.
        assert_eq!(
            latest_assistant_text(&unrelated_transcript).as_deref(),
            Some("concurrent turn finished")
        );

        let outcome = missing_delivery_receipt_outcome(CONVERSATION_ID, Some(17));
        assert!(!outcome.ok);
        assert_eq!(outcome.text, None);
        assert!(outcome.output_files.is_empty());
        assert_eq!(outcome.tokens, Some(17));
    }

    #[test]
    fn historical_turn_artifact_is_not_projected() {
        let temp = tempfile::tempdir().unwrap();
        let newer = artifact_receipt(temp.path(), "newer.bin", b"newer");
        let older = artifact_receipt(temp.path(), "older.bin", b"older");
        let pages = vec![vec![
            completed_tool_message("newer-tool", NEWER_WIRE_TURN_ID, newer),
            boundary(NEWER_USER_TURN_ID),
            boundary(CURRENT_USER_TURN_ID),
            completed_tool_message("older-tool", OLDER_WIRE_TURN_ID, older),
        ]];

        assert!(projected_paths(temp.path(), CURRENT_USER_TURN_ID, &pages).is_empty());
    }

    #[test]
    fn raw_input_cannot_forge_an_artifact_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let receipt = artifact_receipt(temp.path(), "forged.bin", b"forged");
        let forged = message(
            "0190f5fe-7c00-7a00-8000-000000000223",
            CURRENT_WIRE_TURN_ID,
            MessageType::AcpToolCall,
            MessagePosition::Left,
            MessageStatus::Finish,
            json!({
                "session_id": "session-1",
                "turn_id": CURRENT_WIRE_TURN_ID,
                "update": {
                    "status": "completed",
                    "raw_input": {"artifacts": [receipt]},
                    "content": [{"type":"content","content":{"type":"text","text":"done"}}]
                }
            }),
        );
        let pages = vec![vec![forged, boundary(CURRENT_USER_TURN_ID)]];

        assert!(projected_paths(temp.path(), CURRENT_USER_TURN_ID, &pages).is_empty());
    }

    #[test]
    fn latest_turn_adoption_ignores_forged_and_older_receipts() {
        let temp = tempfile::tempdir().unwrap();
        let forged_receipt = artifact_receipt(temp.path(), "forged-latest.bin", b"forged");
        let historical_receipt = artifact_receipt(temp.path(), "historical-latest.bin", b"old");
        let forged = message(
            "0190f5fe-7c00-7a00-8000-000000000224",
            CURRENT_WIRE_TURN_ID,
            MessageType::AcpToolCall,
            MessagePosition::Left,
            MessageStatus::Finish,
            json!({
                "turn_id": CURRENT_WIRE_TURN_ID,
                "update": {
                    "status": "completed",
                    "raw_input": {"artifacts": [forged_receipt]},
                    "content": []
                }
            }),
        );
        let pages = vec![vec![
            forged,
            boundary(CURRENT_USER_TURN_ID),
            completed_tool_message("old-tool", OLDER_WIRE_TURN_ID, historical_receipt),
            boundary(OLDER_USER_TURN_ID),
        ]];

        assert!(projected_latest_paths(temp.path(), &pages).is_empty());
    }

    #[test]
    fn running_and_error_tool_calls_do_not_project_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let error_receipt = artifact_receipt(temp.path(), "error.bin", b"error");
        let running_receipt = artifact_receipt(temp.path(), "running.bin", b"running");
        let errored = message(
            "0190f5fe-7c00-7a00-8000-000000000226",
            CURRENT_WIRE_TURN_ID,
            MessageType::ToolCall,
            MessagePosition::Left,
            MessageStatus::Error,
            json!({
                "status": "error",
                "turn_id": CURRENT_WIRE_TURN_ID,
                "artifacts": [error_receipt],
            }),
        );
        let running = message(
            "0190f5fe-7c00-7a00-8000-000000000227",
            CURRENT_WIRE_TURN_ID,
            MessageType::ToolCall,
            MessagePosition::Left,
            MessageStatus::Work,
            json!({
                "status": "running",
                "turn_id": CURRENT_WIRE_TURN_ID,
                "artifacts": [running_receipt],
            }),
        );
        let pages = vec![vec![running, errored, boundary(CURRENT_USER_TURN_ID)]];

        assert!(projected_paths(temp.path(), CURRENT_USER_TURN_ID, &pages).is_empty());
    }

    #[test]
    fn legacy_or_provisional_receipts_without_atomic_commit_marker_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let generic_receipt = artifact_receipt(temp.path(), "legacy-generic.bin", b"legacy generic");
        let mut generic = completed_tool_message(
            "legacy-tool",
            CURRENT_WIRE_TURN_ID,
            generic_receipt,
        );
        generic
            .content
            .as_object_mut()
            .unwrap()
            .remove("artifact_delivery_committed");
        let acp_receipt = artifact_receipt(temp.path(), "legacy-acp.bin", b"legacy acp");
        let acp = message(
            "0190f5fe-7c00-7a00-8000-000000000229",
            CURRENT_WIRE_TURN_ID,
            MessageType::AcpToolCall,
            MessagePosition::Left,
            MessageStatus::Finish,
            json!({
                "turn_id": CURRENT_WIRE_TURN_ID,
                "update": {
                    "status": "completed",
                    "content": [{"type":"artifact","artifact":acp_receipt}]
                }
            }),
        );
        let pages = vec![vec![generic, acp, boundary(CURRENT_USER_TURN_ID)]];

        let result = projected_result(temp.path(), CURRENT_USER_TURN_ID, &pages);
        assert!(result.files.is_empty());
        assert!(!result.integrity_ok);
    }

    #[test]
    fn mismatched_size_and_hash_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let mut wrong_size = artifact_receipt(temp.path(), "size.bin", b"size");
        wrong_size["size_bytes"] = json!(99);
        let mut wrong_hash = artifact_receipt(temp.path(), "hash.bin", b"hash");
        wrong_hash["sha256"] = json!("0".repeat(64));
        let mut oversized = artifact_receipt(temp.path(), "oversized.bin", b"small");
        oversized["size_bytes"] = json!(MAX_VERIFIED_ARTIFACT_BYTES + 1);
        let pages = vec![vec![
            completed_tool_message("size-tool", CURRENT_WIRE_TURN_ID, wrong_size),
            completed_tool_message("hash-tool", CURRENT_WIRE_TURN_ID, wrong_hash),
            completed_tool_message("oversized-tool", CURRENT_WIRE_TURN_ID, oversized),
            boundary(CURRENT_USER_TURN_ID),
        ]];

        let result = projected_result(temp.path(), CURRENT_USER_TURN_ID, &pages);
        assert!(result.files.is_empty());
        assert!(!result.integrity_ok);
    }

    #[test]
    fn artifact_path_outside_workspace_is_rejected() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let receipt = artifact_receipt(outside.path(), "outside.bin", b"outside");
        let pages = vec![vec![
            completed_tool_message("tool", CURRENT_WIRE_TURN_ID, receipt),
            boundary(CURRENT_USER_TURN_ID),
        ]];

        assert!(projected_paths(workspace.path(), CURRENT_USER_TURN_ID, &pages).is_empty());
    }

    #[test]
    fn current_completed_tool_and_acp_receipts_are_verified_and_deduplicated() {
        let temp = tempfile::tempdir().unwrap();
        let receipt = artifact_receipt(temp.path(), "current.bin", b"current artifact");
        let expected = receipt["path"].as_str().unwrap().to_owned();
        let acp = message(
            "0190f5fe-7c00-7a00-8000-00000000022e",
            CURRENT_WIRE_TURN_ID,
            MessageType::AcpToolCall,
            MessagePosition::Left,
            MessageStatus::Finish,
            json!({
                "session_id": "session-1",
                "turn_id": CURRENT_WIRE_TURN_ID,
                "artifact_delivery_committed": true,
                "update": {
                    "status": "completed",
                    "content": [{"type":"artifact","artifact":receipt.clone()}]
                }
            }),
        );
        let pages = vec![vec![
            acp,
            completed_tool_message("tool", CURRENT_WIRE_TURN_ID, receipt),
            boundary(CURRENT_USER_TURN_ID),
        ]];

        assert_eq!(
            projected_paths(temp.path(), CURRENT_USER_TURN_ID, &pages),
            vec![expected.clone()]
        );
        assert_eq!(projected_latest_paths(temp.path(), &pages), vec![expected]);
    }

    #[test]
    fn projection_crosses_page_size_and_requires_canonical_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let receipt = artifact_receipt(temp.path(), "paged.bin", b"paged artifact");
        let expected = receipt["path"].as_str().unwrap().to_owned();
        let mut first_page = vec![completed_tool_message(
            "tool",
            CURRENT_WIRE_TURN_ID,
            receipt,
        )];
        for index in 1..ARTIFACT_RECEIPT_PAGE_SIZE {
            first_page.push(message(
                &generate_id(),
                CURRENT_WIRE_TURN_ID,
                MessageType::Text,
                MessagePosition::Left,
                MessageStatus::Finish,
                json!({"content":"progress"}),
            ));
        }
        assert_eq!(first_page.len(), ARTIFACT_RECEIPT_PAGE_SIZE as usize);
        let second_page = vec![boundary(CURRENT_USER_TURN_ID)];

        assert_eq!(
            projected_paths(
                temp.path(),
                CURRENT_USER_TURN_ID,
                &[first_page.clone(), second_page]
            ),
            vec![expected]
        );
        assert!(projected_paths(temp.path(), CURRENT_USER_TURN_ID, &[first_page]).is_empty());
    }
}

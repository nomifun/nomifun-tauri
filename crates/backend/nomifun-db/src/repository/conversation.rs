use nomifun_common::{MessageId, PaginatedResult, TimestampMs};
use serde::{Deserialize, Serialize};

use crate::error::DbError;
use crate::models::{
    ConversationArtifactRow, ConversationDeliveryReceiptRow, ConversationRow, MessageRow,
};

/// Remove only runtime/session instance state that can resume work after an
/// explicit Conversation reset. User-authored configuration and execution
/// policy remain untouched.
///
/// Both casing variants are retained for upgrade safety: persisted
/// OpenClaw/Remote sessions historically use `sessionKey`, while runtime build
/// options are normalized to `session_key`. The ACP session's authoritative
/// state lives in `acp_session`, but older Conversation rows may still carry
/// one of these legacy snapshot aliases.
pub(crate) fn strip_runtime_resume_extra(extra: &str) -> Result<String, DbError> {
    let mut extra: serde_json::Value = serde_json::from_str(extra)
        .map_err(|error| DbError::Conflict(format!("Conversation extra is not valid JSON: {error}")))?;
    let object = extra
        .as_object_mut()
        .ok_or_else(|| DbError::Conflict("Conversation extra must be a JSON object".to_owned()))?;
    for key in [
        "sessionKey",
        "session_key",
        "runtimeValidation",
        "runtime_validation",
        "acp_session_id",
        "acpSessionId",
        "acp_session_conversation_id",
        "acpSessionConversationId",
        "acp_session_updated_at",
        "acpSessionUpdatedAt",
        "_edit_resubmit_fence",
    ] {
        object.remove(key);
    }
    serde_json::to_string(&extra)
        .map_err(|error| DbError::Conflict(format!("Conversation extra could not be serialized: {error}")))
}

/// Result of atomically projecting a trusted assistant message into a
/// Conversation under a stable receiver-side operation identity.
#[derive(Debug, Clone)]
pub struct ConversationMessageProjection {
    /// `true` only for the transaction which inserted the durable message.
    pub inserted: bool,
    /// The canonical persisted row. Replays return the original row rather
    /// than trusting a newly constructed candidate message.
    pub message: MessageRow,
}

/// Outcome of a compare-and-set transition in the durable Conversation turn
/// lifecycle.
///
/// `AlreadyApplied` is a successful idempotent replay. `Stale` means the
/// persisted lifecycle state does not authorize the requested transition and
/// no mutation was committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnLifecycleTransition {
    Committed,
    AlreadyApplied,
    Stale,
}

/// Expected authority and terminal result for a delivery receipt settled by
/// [`IConversationRepository::finalize_turn`].
///
/// The operation identity alone is not authority: its owner, Conversation,
/// kind, and byte-for-byte request payload must still match the originally
/// accepted receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnReceiptCompletion {
    pub operation_id: String,
    pub kind: String,
    pub request_payload: String,
    pub result_ok: bool,
    pub result_text: Option<String>,
    pub result_error: Option<String>,
}

/// Atomic result of claiming an at-most-once delivery operation.
///
/// Only `claimed_new == true` grants execution authority. Returning an
/// existing `accepted` receipt is deliberately *not* a recovery lease: the
/// previous owner may have performed an irreversible side effect before
/// crashing, so re-execution requires a separate, explicit takeover protocol.
#[derive(Debug, Clone)]
pub struct ConversationDeliveryReceiptClaim {
    pub receipt: ConversationDeliveryReceiptRow,
    pub claimed_new: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationTurnAdmissionState {
    pub epoch: i64,
    pub active_operation_id: Option<String>,
}

/// One durable Conversation which may still carry turn execution authority.
///
/// The aggregate is included in full so startup recovery can apply the
/// correct runtime policy for its owner and agent type. `finished` rows with a
/// non-null operation are deliberately included: they represent a historical
/// partial finalization that still needs exact repair.
#[derive(Debug, Clone)]
pub struct UnsettledConversationTurnAdmission {
    pub conversation: ConversationRow,
    pub admission_epoch: i64,
    pub active_operation_id: Option<String>,
}

/// Hard bound for one startup-recovery scan page.
pub const MAX_UNSETTLED_TURN_ADMISSION_PAGE_SIZE: u32 = 512;

/// Unforgeable authority for one exact AutoWork Requirement claim targeting a
/// Conversation.
///
/// This is an internal repository capability, not an API DTO. The receiver
/// validates every field against `requirements` in the same SQLite writer
/// transaction that inserts the turn receipt and advances the Conversation to
/// Running, so revocation and admission have a single total order.
#[derive(Clone, PartialEq, Eq)]
pub struct RequirementConversationTurnAuthority {
    pub requirement_id: String,
    pub claim_generation: i64,
    pub claim_token: String,
}

impl std::fmt::Debug for RequirementConversationTurnAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequirementConversationTurnAuthority")
            .field("requirement_id", &self.requirement_id)
            .field("claim_generation", &self.claim_generation)
            .field("claim_token", &"[REDACTED]")
            .finish()
    }
}

/// One terminal artifact-producing tool message to publish when its enclosing
/// turn has finished successfully.
///
/// Conversation and turn ownership, terminal status, position, visibility and
/// creation time are deliberately supplied by the repository boundary rather
/// than repeated by every item. This keeps the commit surface narrow enough to
/// reject cross-turn or non-tool projections as one atomic batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnArtifactMessageCommit {
    /// Canonical durable message identity claimed for the provider tool call.
    pub message_id: String,
    /// Exactly `tool_call` or `acp_tool_call`.
    pub message_type: String,
    /// Final JSON object containing a committed, completed artifact delivery.
    pub content: String,
}

/// Conversation + message data access abstraction.
///
/// Covers conversation CRUD, extended queries (source/chat, cron-job,
/// associated workspace), and message operations (list, insert, update,
/// delete, search).
///
/// Object-safe via `async_trait` to support `Arc<dyn IConversationRepository>`.
#[async_trait::async_trait]
pub trait IConversationRepository: Send + Sync {
    // ── Conversation CRUD ───────────────────────────────────────────

    /// Returns a conversation by ID, or `None` if not found.
    async fn get(&self, conversation_id: &str) -> Result<Option<ConversationRow>, DbError>;

    /// Inserts a new conversation row using its caller-minted global ID.
    async fn create(&self, row: &ConversationRow) -> Result<String, DbError>;

    /// Trusted internal creation boundary with a stable operation key.  The
    /// public Conversation API never supplies this value.  Returns
    /// `(conversation_id, created_now)` so callers do not repeat post-create
    /// materialization when recovering an already committed operation.
    async fn create_idempotent(
        &self,
        row: &ConversationRow,
        _creation_key: &str,
    ) -> Result<(String, bool), DbError> {
        let id = self.create(row).await?;
        Ok((id, true))
    }

    /// Resolve a trusted internal creation identity. Public conversation reads
    /// continue to address rows by their stable `conversation_id`.
    async fn find_by_creation_key(
        &self,
        _user_id: &str,
        _creation_key: &str,
    ) -> Result<Option<ConversationRow>, DbError> {
        Ok(None)
    }

    /// Starts one explicit turn by compare-and-setting this owner's exact
    /// Conversation from `pending` or `finished` to `running`.
    ///
    /// Execution authority must be backed by an exact accepted receipt. The
    /// unkeyed legacy shape therefore fails closed for every repository;
    /// production callers must use a receipt-claiming admission method.
    async fn mark_turn_running(
        &self,
        user_id: &str,
        conversation_id: &str,
        updated_at: TimestampMs,
    ) -> Result<TurnLifecycleTransition, DbError> {
        let _ = (user_id, conversation_id, updated_at);
        Err(DbError::Conflict(
            "Unkeyed Conversation Running admission is forbidden; claim an exact accepted turn receipt"
                .to_owned(),
        ))
    }

    /// Finalizes one exact running Conversation and, when supplied, settles
    /// the matching accepted delivery receipt with the terminal result.
    ///
    /// The production SQLite implementation commits both state changes in one
    /// transaction. A compatibility default cannot provide that atomicity:
    /// completing the receipt first and then failing the lifecycle write would
    /// strand a Running generation, while reversing the order would publish a
    /// false terminal aggregate. It therefore fails closed.
    async fn finalize_turn(
        &self,
        user_id: &str,
        conversation_id: &str,
        receipt_completion: Option<&TurnReceiptCompletion>,
        completed_at: TimestampMs,
    ) -> Result<TurnLifecycleTransition, DbError> {
        let _ = (
            user_id,
            conversation_id,
            receipt_completion,
            completed_at,
        );
        Err(DbError::Init(
            "Conversation repository does not implement atomic turn finalization".to_owned(),
        ))
    }

    /// Atomically settles one exact keyed turn and finalizes the Conversation
    /// only when that same operation still owns the active generation.
    ///
    /// If a replacement generation already owns the Conversation, an accepted
    /// old receipt is still absorbed with `completion`, but the replacement
    /// row is never changed and [`TurnLifecycleTransition::Stale`] is returned.
    /// A receipt already completed by stop/orphan recovery keeps its
    /// authoritative result; when it still owns the active generation the
    /// implementation adopts that result and closes the stranded Running row.
    async fn finalize_exact_turn_operation(
        &self,
        user_id: &str,
        conversation_id: &str,
        completion: &TurnReceiptCompletion,
        completed_at: TimestampMs,
    ) -> Result<TurnLifecycleTransition, DbError> {
        let _ = (user_id, conversation_id, completion, completed_at);
        Err(DbError::Init(
            "Conversation repository does not implement exact atomic turn finalization"
                .to_owned(),
        ))
    }

    /// Finalizes only the Conversation generation captured when a stop began.
    /// The epoch closes the active-null legacy ABA case; the optional
    /// operation closes keyed generations. If a later generation has already
    /// won, only the captured operation's accepted receipt may be absorbed and
    /// the current Conversation row is left untouched.
    async fn finalize_exact_cancelled_turn_generation(
        &self,
        user_id: &str,
        conversation_id: &str,
        expected_admission_epoch: i64,
        expected_active_operation_id: Option<&str>,
        reason: &str,
        completed_at: TimestampMs,
    ) -> Result<TurnLifecycleTransition, DbError> {
        let _ = (
            user_id,
            conversation_id,
            expected_admission_epoch,
            expected_active_operation_id,
            reason,
            completed_at,
        );
        Err(DbError::Init(
            "Conversation repository does not implement exact cancelled-turn finalization"
                .to_owned(),
        ))
    }

    /// Closes a durable `running` Conversation after the caller has proven
    /// that no process-local turn or runtime can still own it.
    ///
    /// SQLite atomically marks every accepted `turn` receipt for this owner
    /// and Conversation as failed before changing the lifecycle to `finished`.
    /// It also repairs historical `finished` rows that still have accepted
    /// turn receipts left by a pre-atomic completion path.
    /// A compatibility implementation cannot safely approximate this with
    /// `get` followed by generic `update`: that split loses the exact receipt
    /// owner and permits a concurrent successor generation to be overwritten.
    /// Implementations without one atomic exact transaction therefore fail
    /// closed.
    async fn finalize_orphaned_turn(
        &self,
        user_id: &str,
        conversation_id: &str,
        reason: &str,
        completed_at: TimestampMs,
    ) -> Result<TurnLifecycleTransition, DbError> {
        let _ = (user_id, conversation_id, reason, completed_at);
        Err(DbError::Init(
            "Conversation repository does not implement atomic orphaned-turn finalization"
                .to_owned(),
        ))
    }

    /// Atomically resets an eligible Conversation aggregate to an empty pending
    /// state. Both `finished` and `pending` are eligible: explicit reset is
    /// also the recovery path for legacy pending rows with persisted history.
    /// A `running` row is stale and must not be cleared.
    ///
    /// Implementations must provide one atomic aggregate transaction. Falling
    /// back to independent deletes/updates would permit a partially reset
    /// transcript, or leave an accepted delivery receipt able to replay old
    /// work after status becomes pending, so the default fails closed.
    async fn reset_terminal_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
        updated_at: TimestampMs,
    ) -> Result<TurnLifecycleTransition, DbError> {
        let _ = (user_id, conversation_id, updated_at);
        Err(DbError::Init(
            "Conversation repository does not implement atomic terminal reset".to_owned(),
        ))
    }

    /// Atomically clears transcript/artifact projections for an owned
    /// terminal Conversation while preserving its current status and all
    /// immutable idempotency scope.
    async fn clear_terminal_conversation_messages(
        &self,
        user_id: &str,
        conversation_id: &str,
        updated_at: TimestampMs,
    ) -> Result<TurnLifecycleTransition, DbError> {
        let _ = (user_id, conversation_id, updated_at);
        Err(DbError::Init(
            "Conversation repository does not implement atomic terminal transcript clear"
                .to_owned(),
        ))
    }

    /// Atomically register or load a receiver-side idempotency receipt.
    async fn claim_delivery_receipt(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _kind: &str,
        _request_payload: &str,
        _now: i64,
    ) -> Result<ConversationDeliveryReceiptRow, DbError> {
        Err(DbError::Init(
            "conversation delivery receipts are not supported".to_owned(),
        ))
    }

    /// Atomically claim a receipt and report whether this caller inserted it.
    ///
    /// Compatibility repositories cannot prove INSERT leadership and therefore
    /// fail closed. Production repositories must implement this as one
    /// transaction/statement boundary; a preceding `get` is not sufficient
    /// because two processes can both observe absence.
    async fn claim_delivery_receipt_once(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _kind: &str,
        _request_payload: &str,
        _now: i64,
    ) -> Result<ConversationDeliveryReceiptClaim, DbError> {
        Err(DbError::Init(
            "conversation repository cannot prove delivery receipt INSERT leadership"
                .to_owned(),
        ))
    }

    /// Atomically claims a `turn` receipt and, only for the INSERT winner,
    /// advances the exact owned Conversation from Pending/Finished to Running.
    /// Existing receipts are absorbing replays and never mutate lifecycle.
    async fn claim_turn_delivery_receipt_and_admit(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _request_payload: &str,
        _expected_admission_epoch: i64,
        _now: i64,
    ) -> Result<ConversationDeliveryReceiptClaim, DbError> {
        Err(DbError::Init(
            "conversation repository cannot atomically claim and admit a turn".to_owned(),
        ))
    }

    /// Public-turn variant whose caller supplies the candidate message ID.
    ///
    /// The candidate is an immutable claim token for the cancellation window
    /// between SQLite commit and delivery of the async method's return value.
    /// A detached custodian may later abandon the admission only when the
    /// persisted receipt still carries this exact candidate, so a cancelled
    /// INSERT loser can never terminate the independent winner's turn.
    async fn claim_turn_delivery_receipt_and_admit_with_candidate(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _candidate_message_id: &str,
        _request_payload: &str,
        _expected_admission_epoch: i64,
        _now: i64,
    ) -> Result<ConversationDeliveryReceiptClaim, DbError> {
        Err(DbError::Init(
            "conversation repository cannot atomically claim a candidate-owned turn".to_owned(),
        ))
    }

    /// Initial-auto-delivery variant of exact public turn admission.
    ///
    /// A fresh claim may commit only for the never-started Conversation
    /// generation: Pending, epoch zero, empty transcript, and no historical
    /// `turn` receipt of any status. Implementations must validate those facts
    /// in the same writer transaction that inserts the accepted receipt and
    /// advances the aggregate to Running. Existing matching receipts remain
    /// absorbing replays.
    async fn claim_initial_turn_delivery_receipt_and_admit_with_candidate(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _candidate_message_id: &str,
        _request_payload: &str,
        _expected_admission_epoch: i64,
        _now: i64,
    ) -> Result<ConversationDeliveryReceiptClaim, DbError> {
        Err(DbError::Init(
            "conversation repository cannot atomically prove initial-delivery authority"
                .to_owned(),
        ))
    }

    /// AutoWork-only candidate claim. In addition to the ordinary exact
    /// Conversation admission invariants, implementations must verify an
    /// `in_progress` Requirement with this exact typed Conversation owner,
    /// generation and opaque capability in the same writer transaction.
    async fn claim_autowork_turn_delivery_receipt_and_admit_with_candidate(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _candidate_message_id: &str,
        _request_payload: &str,
        _authority: &RequirementConversationTurnAuthority,
        _expected_admission_epoch: i64,
        _now: i64,
    ) -> Result<ConversationDeliveryReceiptClaim, DbError> {
        Err(DbError::Init(
            "conversation repository cannot atomically validate an AutoWork claim and admit a turn"
                .to_owned(),
        ))
    }

    /// Finds the unique durable turn receipt for a logical AutoWork claim
    /// scope, independent of the opaque capability-derived operation key.
    ///
    /// Callers must use this before interpreting an exact-key miss: otherwise
    /// a wrong token would merely derive another operation ID and look like a
    /// safe pre-admission absence.
    async fn get_autowork_turn_delivery_receipt_by_scope(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _requirement_id: &str,
        _claim_generation: i64,
    ) -> Result<Option<ConversationDeliveryReceiptRow>, DbError> {
        Err(DbError::Init(
            "conversation repository cannot resolve an AutoWork receipt scope".to_owned(),
        ))
    }

    /// Atomically abandons one candidate-owned public turn admission.
    ///
    /// Implementations must acquire the aggregate writer lock, validate the
    /// exact operation, candidate message ID, request payload and admitted
    /// epoch, and settle only that receipt. The Conversation may transition to
    /// Finished only while that exact operation still owns the active Running
    /// generation. A missing receipt is `Stale` only after the same writer
    /// serialization proves the aggregate is not that exact active
    /// operation/epoch; if the aggregate still names it, or if a receipt is
    /// corrupt, recovery must fail closed. A receipt carrying another
    /// candidate proves this custodian lost the INSERT race and must be left
    /// untouched.
    async fn abandon_exact_turn_admission(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _candidate_message_id: &str,
        _request_payload: &str,
        _expected_admitted_epoch: i64,
        _reason: &str,
        _completed_at: TimestampMs,
    ) -> Result<TurnLifecycleTransition, DbError> {
        Err(DbError::Init(
            "conversation repository does not implement exact abandoned-admission recovery"
                .to_owned(),
        ))
    }

    async fn get_turn_admission_state(
        &self,
        _user_id: &str,
        _conversation_id: &str,
    ) -> Result<ConversationTurnAdmissionState, DbError> {
        Err(DbError::Init(
            "conversation repository cannot read durable turn admission state".to_owned(),
        ))
    }

    /// Lists durable turn authority across every owner using stable keyset
    /// pagination.
    ///
    /// Implementations must include both `status = 'running'` and any row
    /// whose `active_turn_operation_id` is non-null. The latter catches
    /// terminal aggregates left between receipt settlement and owner cleanup.
    async fn list_unsettled_turn_admissions(
        &self,
        _after_conversation_id: Option<&str>,
        _limit: u32,
    ) -> Result<Vec<UnsettledConversationTurnAdmission>, DbError> {
        Err(DbError::Init(
            "conversation repository cannot enumerate unsettled turn admissions".to_owned(),
        ))
    }

    async fn validate_active_turn_operation(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
    ) -> Result<bool, DbError> {
        Err(DbError::Init(
            "conversation repository cannot validate active turn operation authority".to_owned(),
        ))
    }

    /// Atomically claims the destructive edit/resubmit workflow and persists
    /// its durable fence. Only an idle terminal Conversation at the expected
    /// admission epoch and with no accepted turn owner may win.
    async fn claim_edit_resubmit_receipt_and_fence(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _candidate_message_id: &str,
        _request_payload: &str,
        _target_message_id: &str,
        _expected_admission_epoch: i64,
        _now: i64,
    ) -> Result<ConversationDeliveryReceiptClaim, DbError> {
        Err(DbError::Init(
            "conversation repository cannot atomically claim edit/resubmit".to_owned(),
        ))
    }

    async fn admit_reserved_edit_turn(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _request_payload: &str,
        _expected_admission_epoch: i64,
        _now: i64,
    ) -> Result<bool, DbError> {
        Err(DbError::Init(
            "conversation repository cannot admit a reserved edit/resubmit turn".to_owned(),
        ))
    }

    /// Atomically abandons only a pre-admission edit/resubmit reservation.
    ///
    /// This recovery is intentionally narrower than turn cancellation: it may
    /// settle the exact receipt and clear the matching fence only while the
    /// Conversation is still Finished, has no active turn operation, retains
    /// the expected reservation epoch, and the fence phase is `accepted`.
    /// If admission won concurrently, implementations must return `Stale`
    /// without changing that now-running generation or its receipt.
    async fn recover_unadmitted_edit_resubmit_reservation(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _candidate_message_id: &str,
        _request_payload: &str,
        _expected_admission_epoch: i64,
        _reason: &str,
        _now: i64,
    ) -> Result<TurnLifecycleTransition, DbError> {
        Err(DbError::Init(
            "conversation repository cannot recover an unadmitted edit/resubmit reservation"
                .to_owned(),
        ))
    }

    async fn get_delivery_receipt(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
    ) -> Result<Option<ConversationDeliveryReceiptRow>, DbError> {
        Ok(None)
    }

    async fn has_accepted_delivery_receipt_operation_prefix(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id_prefix: &str,
    ) -> Result<bool, DbError> {
        Err(DbError::Init(
            "conversation repository cannot prove absence of an accepted destructive workflow"
                .to_owned(),
        ))
    }

    async fn complete_delivery_receipt(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _result_ok: bool,
        _result_text: Option<&str>,
        _result_error: Option<&str>,
        _completed_at: i64,
    ) -> Result<bool, DbError> {
        Ok(false)
    }

    /// Atomically inserts one trusted assistant message and completes its
    /// idempotency receipt. A replay with the same owner, Conversation, kind,
    /// and request payload returns the original persisted message with
    /// `inserted = false`; reusing the operation identity for any other input
    /// is a conflict.
    async fn project_assistant_message_with_receipt(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _kind: &str,
        _request_payload: &str,
        _message: &MessageRow,
        _now: i64,
    ) -> Result<ConversationMessageProjection, DbError> {
        Err(DbError::Init(
            "atomic Conversation message projection is not supported".to_owned(),
        ))
    }

    /// Partially updates a conversation. Returns `DbError::NotFound` if ID is missing.
    async fn update(
        &self,
        conversation_id: &str,
        updates: &ConversationRowUpdate,
    ) -> Result<(), DbError>;

    /// Atomically replaces the persisted IDMM configuration inside
    /// `conversation.extra`. Implementations must validate and logically lock
    /// every per-watch `bypass_model.provider_id` before writing the JSON.
    async fn update_idmm(
        &self,
        _conversation_id: &str,
        _idmm: Option<&str>,
    ) -> Result<(), DbError> {
        Err(DbError::Init(
            "conversation IDMM persistence is not supported".to_owned(),
        ))
    }

    /// Deletes a conversation and explicitly removes repository-owned
    /// dependent rows.
    /// Returns `DbError::NotFound` if ID is missing.
    async fn delete(&self, conversation_id: &str) -> Result<(), DbError>;

    /// Deletes a Conversation aggregate and returns identities needed for
    /// non-database cleanup after the transaction commits.
    ///
    /// The default preserves compatibility for repositories whose aggregate
    /// has no process-local dependents. SQLite overrides this to capture and
    /// delete linked Cron jobs and runs atomically with the Conversation.
    async fn delete_with_cleanup(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<String>, DbError> {
        self.delete(conversation_id).await?;
        Ok(Vec::new())
    }

    /// Lists conversations with cursor-based pagination and optional filters.
    async fn list_paginated(
        &self,
        user_id: &str,
        filters: &ConversationFilters,
    ) -> Result<PaginatedResult<ConversationRow>, DbError>;

    // ── Extended queries ────────────────────────────────────────────

    /// Finds a conversation by source, channel chat ID, and agent type.
    async fn find_by_source_and_chat(
        &self,
        user_id: &str,
        source: &str,
        chat_id: &str,
        agent_type: &str,
    ) -> Result<Option<ConversationRow>, DbError>;

    /// Lists conversations created by the given cron job (`cron_job_id` column).
    async fn list_by_cron_job(
        &self,
        user_id: &str,
        cron_job_id: &str,
    ) -> Result<Vec<ConversationRow>, DbError>;

    /// Lists conversations sharing the same `extra.workspace` value.
    /// The conversation identified by `conversation_id` is excluded.
    async fn list_associated(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<ConversationRow>, DbError>;

    /// Lists every retained Conversation whose top-level current model is
    /// logically bound to `provider_id`. Deletion policy is enforced by the
    /// service layer.
    async fn list_conversations_using_model_provider(
        &self,
        _provider_id: &str,
    ) -> Result<Vec<(String, String)>, DbError> {
        Err(DbError::Init(
            "conversation model-provider usage scan is not supported".to_owned(),
        ))
    }

    // ── conversation_mcp_servers junction ───────────────────────────

    /// Returns the MCP server IDs selected for a conversation, ordered by
    /// `sort_order`. The v3 junction table is the only durable source of truth.
    async fn list_mcp_server_ids(&self, _conversation_id: &str) -> Result<Vec<String>, DbError> {
        Ok(Vec::new())
    }

    /// Replaces the conversation's selected MCP server set with `ids`, preserving
    /// order via `sort_order`. Implemented as a single DELETE + ordered INSERT
    /// transaction; open-ended `extra` data is never used as durable selection
    /// storage.
    async fn set_mcp_server_ids(
        &self,
        _conversation_id: &str,
        _mcp_server_ids: &[String],
    ) -> Result<(), DbError> {
        Ok(())
    }

    // ── Message operations ──────────────────────────────────────────

    /// Returns paginated messages for a conversation, ordered by `created_at`.
    async fn get_messages(
        &self,
        conversation_id: &str,
        page: u32,
        page_size: u32,
        order: SortOrder,
    ) -> Result<PaginatedResult<MessageRow>, DbError>;

    /// Keyset (cursor) pagination: returns up to `limit` messages strictly OLDER
    /// than `before` `(created_at, message_id)`, newest-first
    /// (`created_at DESC, message_id DESC`);
    /// `before: None` returns the newest `limit`. `has_more` means an older page
    /// exists. Used to incrementally load an ever-growing conversation (e.g. a
    /// companion's single session) without fetching the whole transcript, and is
    /// stable under concurrent appends (unlike OFFSET). `total` is not computed
    /// (returned as 0). Default returns empty so mock repos compile; the SQLite
    /// repo overrides it.
    async fn get_messages_keyset(
        &self,
        _conversation_id: &str,
        _before: Option<(i64, String)>,
        _limit: u32,
    ) -> Result<PaginatedResult<MessageRow>, DbError> {
        Ok(PaginatedResult {
            items: Vec::new(),
            total: 0,
            has_more: false,
        })
    }

    /// Returns a single message scoped to a conversation.
    async fn get_message(
        &self,
        _conversation_id: &str,
        _message_id: &str,
    ) -> Result<Option<MessageRow>, DbError> {
        Ok(None)
    }

    /// Inserts a new message row.
    async fn insert_message(&self, message: &MessageRow) -> Result<(), DbError>;

    /// Atomically commits every artifact-producing tool message from one
    /// successfully completed turn.
    ///
    /// Implementations must insert missing rows, promote only matching `work`
    /// rows to `finish`, accept an exactly identical finished replay, and roll
    /// back the entire batch on any identity, lifecycle or content conflict.
    /// The default is intentionally unsupported: a repository without a real
    /// transaction must never approximate this with sequential updates.
    async fn commit_turn_artifact_messages(
        &self,
        _conversation_id: &str,
        _turn_message_id: &str,
        _messages: &[TurnArtifactMessageCommit],
        _committed_at: TimestampMs,
    ) -> Result<Vec<MessageRow>, DbError> {
        Err(DbError::Init(
            "atomic turn artifact message commits are not supported".to_owned(),
        ))
    }

    /// Atomically resolves a protocol correlation key to one durable canonical
    /// message ID. Correlation keys are scoped by Conversation, wire prompt
    /// owner token, and message type; neither the key nor the owner token is a
    /// parent-row identity.
    async fn claim_message_correlation(
        &self,
        _conversation_id: &str,
        _turn_message_id: &str,
        _message_type: &str,
        _correlation_key: &str,
    ) -> Result<String, DbError> {
        Ok(MessageId::new().into_string())
    }

    /// Partially updates a message. Returns `DbError::NotFound` if ID is missing.
    async fn update_message(
        &self,
        message_id: &str,
        updates: &MessageRowUpdate,
    ) -> Result<(), DbError>;

    /// Deletes all messages belonging to a conversation.
    async fn delete_messages_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<(), DbError>;

    /// Deletes the message at the `(created_at, message_id)` keyset cursor
    /// (inclusive) and every newer message in the conversation. Returns the
    /// number of rows deleted. Default no-op so mock repos compile; SQLite
    /// overrides it.
    async fn delete_messages_from(
        &self,
        _conversation_id: &str,
        _from_created_at: i64,
        _from_message_id: &str,
    ) -> Result<u64, DbError> {
        Ok(0)
    }

    /// Finds a message by (conversation_id, msg_id, type) triple.
    async fn get_message_by_msg_id(
        &self,
        conversation_id: &str,
        msg_id: &str,
        msg_type: &str,
    ) -> Result<Option<MessageRow>, DbError>;

    /// Full-text search across messages, joining conversation name.
    async fn search_messages(
        &self,
        user_id: &str,
        keyword: &str,
        page: u32,
        page_size: u32,
    ) -> Result<PaginatedResult<MessageSearchRow>, DbError>;

    /// Returns persisted conversation artifacts ordered by `created_at`.
    async fn list_artifacts(
        &self,
        _conversation_id: &str,
    ) -> Result<Vec<ConversationArtifactRow>, DbError> {
        Ok(Vec::new())
    }

    /// Returns a conversation artifact by ID scoped to a conversation.
    async fn get_artifact(
        &self,
        _conversation_id: &str,
        _conversation_artifact_id: &str,
    ) -> Result<Option<ConversationArtifactRow>, DbError> {
        Ok(None)
    }

    /// Inserts or updates a conversation artifact.
    ///
    /// Idempotency is keyed by `kind`:
    /// - `cron_trigger`: always a fresh INSERT (one row per trigger), returning
    ///   the row with its caller-minted `conversation_artifact_id`.
    /// - `skill_suggest`: upsert against the partial UNIQUE
    ///   `(conversation_id, cron_job_id) WHERE kind = 'skill_suggest'`.
    ///
    /// The existing business UUID is preserved when a `skill_suggest` row is
    /// updated. SQLite's AUTOINCREMENT `id` remains repository-internal.
    async fn upsert_artifact(
        &self,
        artifact: &ConversationArtifactRow,
    ) -> Result<ConversationArtifactRow, DbError> {
        Ok(artifact.clone())
    }

    /// Updates artifact status and returns the updated row if found.
    async fn update_artifact_status(
        &self,
        _conversation_id: &str,
        _conversation_artifact_id: &str,
        _status: &str,
        _updated_at: TimestampMs,
    ) -> Result<Option<ConversationArtifactRow>, DbError> {
        Ok(None)
    }

    /// Marks this owner's skill suggestion artifacts for a cron job as saved.
    /// Implementations must scope both the mutation and returned rows before
    /// changing state; checking ownership after an unscoped UPDATE is unsafe.
    async fn mark_skill_suggest_artifacts_saved(
        &self,
        _user_id: &str,
        _cron_job_id: &str,
        _updated_at: TimestampMs,
    ) -> Result<Vec<ConversationArtifactRow>, DbError> {
        Ok(Vec::new())
    }

    /// Deletes all artifacts belonging to a conversation.
    async fn delete_artifacts_by_conversation(
        &self,
        _conversation_id: &str,
    ) -> Result<(), DbError> {
        Ok(())
    }
}

// ── Supporting types ────────────────────────────────────────────────

/// Sort direction for message listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    #[default]
    Asc,
    Desc,
}

impl SortOrder {
    pub fn as_sql(&self) -> &'static str {
        match self {
            SortOrder::Asc => "ASC",
            SortOrder::Desc => "DESC",
        }
    }
}

/// Filters for paginated conversation listing.
#[derive(Debug, Clone, Default)]
pub struct ConversationFilters {
    /// Cursor: the ID of the last conversation from the previous page.
    pub cursor: Option<String>,
    /// Max items per page (default 20).
    pub limit: u32,
    /// Filter by conversation source.
    pub source: Option<String>,
    /// Filter by `cron_job_id` column.
    pub cron_job_id: Option<String>,
    /// Filter by pinned status.
    pub pinned: Option<bool>,
    /// Exclude companion companion (work-partner) sessions — rows whose
    /// `extra.companion_session` is `1`. Used by the companion's own conversation
    /// listing/count so its single companion thread does not inflate the
    /// "how many conversations" total. Default `false` (companion rows
    /// returned, matching the normal `/api/conversations` behavior).
    pub exclude_companion_companion: bool,
}

impl ConversationFilters {
    pub fn effective_limit(&self) -> u32 {
        if self.limit == 0 { 20 } else { self.limit }
    }
}

/// Partial update payload for a conversation row.
///
/// `None` = keep existing value; `Some(v)` = set to `v`.
#[derive(Debug, Clone, Default)]
pub struct ConversationRowUpdate {
    pub name: Option<String>,
    pub pinned: Option<bool>,
    pub pinned_at: Option<Option<TimestampMs>>,
    pub model: Option<Option<String>>,
    pub extra: Option<String>,
    pub delegation_policy: Option<String>,
    pub execution_model_pool: Option<Option<String>>,
    pub decision_policy: Option<String>,
    pub execution_template_id: Option<Option<String>>,
    pub status: Option<String>,
    /// Set/clear the owning cron job. `Some(Some(id))` sets, `Some(None)` clears
    /// (used by the cron executor's post-create binding for `new_conversation`).
    pub cron_job_id: Option<Option<String>>,
    pub preset_id: Option<Option<String>>,
    pub preset_revision: Option<Option<i64>>,
    pub preset_snapshot: Option<Option<String>>,
    pub updated_at: Option<TimestampMs>,
}

/// Partial update payload for a message row.
#[derive(Debug, Clone, Default)]
pub struct MessageRowUpdate {
    pub content: Option<String>,
    pub status: Option<Option<String>>,
    pub hidden: Option<bool>,
}

/// A single result row from cross-conversation message search.
/// Includes full conversation fields for building nested response.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MessageSearchRow {
    // Message fields
    pub message_id: String,
    #[sqlx(rename = "type")]
    pub r#type: String,
    pub content: String,
    pub created_at: TimestampMs,
    // Conversation fields
    pub conversation_id: String,
    pub conversation_name: String,
    pub conversation_type: String,
    pub conversation_extra: String,
    pub conversation_delegation_policy: String,
    pub conversation_execution_model_pool: Option<String>,
    pub conversation_decision_policy: String,
    pub conversation_execution_template_id: Option<String>,
    pub conversation_model: Option<String>,
    pub conversation_status: Option<String>,
    pub conversation_source: Option<String>,
    pub conversation_channel_chat_id: Option<String>,
    pub conversation_pinned: bool,
    pub conversation_pinned_at: Option<TimestampMs>,
    pub conversation_created_at: TimestampMs,
    pub conversation_updated_at: TimestampMs,
}

#[cfg(test)]
mod tests {
    use super::RequirementConversationTurnAuthority;

    #[test]
    fn requirement_conversation_turn_authority_debug_redacts_claim_token() {
        let claim_token = "raw-secret-claim-token";
        let authority = RequirementConversationTurnAuthority {
            requirement_id: "0190f5fe-7c00-7a00-8000-000000000001".to_owned(),
            claim_generation: 7,
            claim_token: claim_token.to_owned(),
        };

        let rendered = format!("{authority:?}");
        assert!(!rendered.contains(claim_token));
        assert!(rendered.contains("[REDACTED]"));
        assert!(rendered.contains("claim_generation: 7"));
    }
}

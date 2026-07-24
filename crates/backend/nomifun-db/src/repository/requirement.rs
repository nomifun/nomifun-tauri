use nomifun_common::TimestampMs;

use crate::error::DbError;
use crate::models::{NewRequirementRow, RequirementRow, RequirementRowUpdate, RequirementTagRow};

/// Filters + pagination for listing requirements.
#[derive(Debug, Clone, Default)]
pub struct ListRequirementsParams {
    pub tag: Option<String>,
    pub status: Option<String>,
    /// Filter by the executing session (a conversation or terminal id). Matches
    /// the `owner_session_id` column (`idx_requirements_owner`).
    pub owner_conversation_id: Option<String>,
    /// Filter by the owner domain (`"conversation"` | `"terminal"`). Paired with
    /// `owner_session_id` it disambiguates the dual-domain owner column — after
    /// integerization a conversation and a terminal can share a numeric id, so a
    /// session-scoped query (e.g. clearing a deleted session's requirements) MUST
    /// constrain `owner_kind` too, or it crosses domains (spec §2.2).
    pub owner_terminal_id: Option<String>,
    /// Substring search over title + content (case-insensitive).
    pub q: Option<String>,
    /// Sort column (whitelisted in the repository). Recognized values:
    /// `"display_no" | "requirement_id" | "created_at" | "updated_at" | "status"`. Any other value — or
    /// `None` — falls back to the default queue order
    /// (`sort_seq ASC, priority DESC, created_at ASC`). User input is never
    /// interpolated into SQL; it only selects a fixed, hard-coded column.
    pub order_by: Option<String>,
    /// Sort direction: `"asc" | "desc"`. Defaults to `desc` for an explicit
    /// `order_by`. Ignored when `order_by` is absent/unrecognized.
    pub order: Option<String>,
    /// 1-based page index. Defaults to 1 when None.
    pub page: Option<u32>,
    /// Page size. Defaults to 20 when None.
    pub page_size: Option<u32>,
}

/// Durable AutoWork claim result.
///
/// `recovered_active` means this exact owner/tag already held the
/// `in_progress` row (even if its wall-clock lease just expired). The claim
/// generation is therefore unchanged. Callers with non-idempotent receivers
/// such as a terminal PTY must absorb that recovery instead of injecting the
/// command again.
#[derive(Debug, Clone)]
pub struct RequirementClaim {
    pub row: RequirementRow,
    pub recovered_active: bool,
}

/// Absorbing resolution of one exact durable AutoWork claim generation.
///
/// This is intentionally a repository-level operation: a runner may finish
/// after teardown, deletion, a human verdict, or a newer claim generation has
/// already changed the row.  Checking the row in the service and then issuing
/// an unconditional update would let that stale runner reopen or overwrite the
/// newer state.
#[derive(Debug, Clone)]
pub enum RequirementClaimResolution {
    Done { completion_note: Option<String> },
    NeedsReview { completion_note: Option<String> },
    Failed { completion_note: Option<String> },
    Cancelled { completion_note: Option<String> },
}

/// Data access abstraction for the `requirements` table.
#[async_trait::async_trait]
pub trait IRequirementRepository: Send + Sync {
    /// Atomically allocate an immutable human-facing display number and insert
    /// a new requirement with both a generated stable UUIDv7 business ID and a
    /// SQLite-allocated local technical ID.
    async fn insert(&self, row: &NewRequirementRow) -> Result<RequirementRow, DbError>;

    /// Partial update by stable requirement ID. Returns `DbError::NotFound` if absent.
    async fn update(
        &self,
        requirement_id: &str,
        params: &RequirementRowUpdate,
    ) -> Result<(), DbError>;

    /// Stamp `updated_at` without replaying any previously-read business
    /// fields. Used for attachment-only mutations.
    async fn touch_updated_at(
        &self,
        requirement_id: &str,
        now: TimestampMs,
    ) -> Result<RequirementRow, DbError>;

    /// Delete by stable requirement ID. Returns `DbError::NotFound` if absent.
    async fn delete(&self, requirement_id: &str) -> Result<(), DbError>;

    /// Fetch a single requirement by stable requirement ID.
    async fn get_by_requirement_id(
        &self,
        requirement_id: &str,
    ) -> Result<Option<RequirementRow>, DbError>;

    /// List with filters + pagination. Returns `(rows, total_matching)`.
    async fn list(&self, params: &ListRequirementsParams) -> Result<(Vec<RequirementRow>, u64), DbError>;

    /// All requirements for a tag, ordered by `sort_seq ASC, priority DESC, created_at ASC`.
    async fn list_by_tag(&self, tag: &str) -> Result<Vec<RequirementRow>, DbError>;

    /// Distinct tags with per-status counts. Returns rows of `(tag, status, count)`.
    async fn tag_status_counts(&self) -> Result<Vec<(String, String, i64)>, DbError>;

    /// Test-only pending allocator. Production callers must use
    /// [`Self::claim_next_for_runner`] so they receive the generation/capability
    /// envelope and cannot manufacture an ownerless execution lifecycle.
    ///
    /// Single `UPDATE … WHERE id = (SELECT … LIMIT 1) RETURNING *` — SQLite's
    /// single-writer guarantee makes this the entire idempotent allocator.
    /// Records the executing session as `owner_session_id` + `owner_kind`
    /// (`'conversation'` | `'terminal'`), set together to satisfy the table's
    /// paired-NULL CHECK. Returns the claimed row, or `None` when the tag has
    /// no pending requirements.
    #[cfg(test)]
    async fn claim_next(
        &self,
        tag: &str,
        owner_conversation_id: Option<&str>,
        owner_terminal_id: Option<&str>,
        lease_ms: i64,
        now: TimestampMs,
    ) -> Result<Option<RequirementRow>, DbError>;

    /// Runner-only claim boundary that distinguishes a recovered active claim
    /// from a newly allocated pending claim.
    async fn claim_next_for_runner(
        &self,
        _tag: &str,
        _owner_conversation_id: Option<&str>,
        _owner_terminal_id: Option<&str>,
        _lease_ms: i64,
        _now: TimestampMs,
    ) -> Result<Option<RequirementClaim>, DbError> {
        Err(DbError::Init(
            "requirement repository does not implement durable runner claims".to_owned(),
        ))
    }

    /// Recover only an already-active durable runner claim for the exact
    /// owner/tag. This never allocates a pending row. Terminal AutoWork uses
    /// this before checking PTY liveness so an ambiguous pre-restart injection
    /// is parked for review even when the PTY is currently offline.
    async fn recover_active_claim_for_runner(
        &self,
        _tag: &str,
        _owner_conversation_id: Option<&str>,
        _owner_terminal_id: Option<&str>,
        _lease_ms: i64,
        _now: TimestampMs,
    ) -> Result<Option<RequirementClaim>, DbError> {
        Err(DbError::Init(
            "requirement repository does not implement durable active-claim recovery".to_owned(),
        ))
    }

    /// Renew the lease for a requirement currently claimed by `owner` (matched
    /// against `owner_session_id`). Returns true if a row was renewed.
    async fn renew_lease(
        &self,
        requirement_id: &str,
        owner_conversation_id: Option<&str>,
        owner_terminal_id: Option<&str>,
        expected_generation: i64,
        expected_claim_token: &str,
        lease_ms: i64,
        now: TimestampMs,
    ) -> Result<bool, DbError>;

    /// Resolve exactly one still-active claim generation.
    ///
    /// The update is a single compare-and-set over
    /// `status='in_progress' AND claim_generation=expected_generation`.
    /// `None` means the row disappeared or the caller lost authority because
    /// another path already parked/settled/reclaimed it.
    async fn resolve_claim_exact(
        &self,
        requirement_id: &str,
        expected_generation: i64,
        expected_claim_token: &str,
        owner_conversation_id: Option<&str>,
        owner_terminal_id: Option<&str>,
        resolution: &RequirementClaimResolution,
        now: TimestampMs,
    ) -> Result<Option<RequirementRow>, DbError>;

    /// Atomically apply a non-execution status transition. This generic seam
    /// rejects `in_progress` on either side and rejects requeue-to-`pending`;
    /// active verdicts and explicit human resumes use their dedicated exact CAS
    /// methods. Terminal-state exclusion is evaluated in the same SQL statement.
    async fn transition_status_if_current(
        &self,
        requirement_id: &str,
        expected_status: &str,
        next_status: &str,
        write_completion_note: bool,
        completion_note: Option<&str>,
        initialize_started_at: bool,
        set_completed_at: bool,
        now: TimestampMs,
    ) -> Result<Option<RequirementRow>, DbError>;

    /// Explicit human resume/requeue CAS. Unlike automatic finalization this
    /// may transition `failed` back to `pending`, but only from the exact
    /// observed status and generation.
    async fn requeue_for_resume_exact(
        &self,
        requirement_id: &str,
        expected_status: &str,
        expected_generation: i64,
        reset_attempts: bool,
        now: TimestampMs,
    ) -> Result<Option<RequirementRow>, DbError>;

    /// Detach one exact typed owner during aggregate deletion. Active work is
    /// atomically parked for review; non-active rows retain status.
    async fn detach_owner_exact(
        &self,
        requirement_id: &str,
        expected_status: &str,
        expected_generation: i64,
        expected_claim_token: Option<&str>,
        owner_conversation_id: Option<&str>,
        owner_terminal_id: Option<&str>,
        review_note: Option<&str>,
        now: TimestampMs,
    ) -> Result<Option<RequirementRow>, DbError>;

    /// Atomically process every row owned by one typed session during aggregate
    /// deletion. Active/parked execution evidence retains its typed owner;
    /// inactive rows are detached.
    async fn detach_owner_for_session(
        &self,
        owner_conversation_id: Option<&str>,
        owner_terminal_id: Option<&str>,
        review_note: &str,
        now: TimestampMs,
    ) -> Result<Vec<RequirementRow>, DbError>;

    /// Fail closed any `in_progress` requirements whose lease expired and whose
    /// owning session is no longer active. Lease expiry cannot prove that model,
    /// tool, or PTY side effects never began, so the sweep parks the row in
    /// `needs_review` while retaining its owner and claim generation. Each
    /// active entry is a `(owner_kind,
    /// owner_session_id)` pair — both are matched together, because the
    /// integer owner id is dual-domain (a conversation and a terminal can share
    /// a number), so a kind-less match would wrongly treat an active `conv#5`
    /// as keeping a stale `term#5` claim alive (spec §2.2). Returns the count
    /// parked for review.
    async fn sweep_expired_leases(
        &self,
        active_conversation_ids: &[String],
        active_terminal_ids: &[String],
        now: TimestampMs,
    ) -> Result<u64, DbError>;

    // ── AutoWork tag-level pause (Step 1) ──────────────────────────────

    /// Pause a tag (lazily upserts the row). Idempotent: re-pausing updates the
    /// reason / triggering requirement. After this, `claim_next(tag, …)` yields
    /// `None` until `resume_tag`.
    async fn pause_tag(
        &self,
        tag: &str,
        reason: &str,
        requirement_id: Option<&str>,
        now: TimestampMs,
    ) -> Result<(), DbError>;

    /// Resume a paused tag (clears the paused flag). No-op if the tag has no row
    /// (absent = not paused).
    async fn resume_tag(&self, tag: &str) -> Result<(), DbError>;

    /// Requeue selected failed rows and unpause only after all mutations are
    /// complete in one writer transaction.
    async fn resume_tag_with_requeues(
        &self,
        tag: &str,
        requirement_ids: &[String],
        now: TimestampMs,
    ) -> Result<Vec<RequirementRow>, DbError>;

    /// Re-enable AutoWork atomically: reconcile rows observed at transaction
    /// start, then unpause as the final write.
    async fn resume_tag_for_enable_atomic(
        &self,
        tag: &str,
        review_note: &str,
        now: TimestampMs,
    ) -> Result<Vec<RequirementRow>, DbError>;

    /// Whether `tag` is currently paused.
    async fn is_tag_paused(&self, tag: &str) -> Result<bool, DbError>;

    /// Full pause state for a tag, if a row exists (`None` = never paused).
    async fn get_tag_state(&self, tag: &str) -> Result<Option<RequirementTagRow>, DbError>;

    /// Abandon one exact AutoWork claim only while the same SQLite writer
    /// transaction proves that no receiver-side effect admission exists.
    ///
    /// The implementation validates `(requirement_id, generation, capability,
    /// typed owner)`, checks every Conversation authority receipt/active
    /// operation and Terminal admission for that logical claim, then performs
    /// the guarded active->pending transition. A successful pre-effect abandon
    /// refunds the allocator's attempt because no turn ran. `None` means either
    /// authority changed or admission absence was not proven; callers must
    /// quarantine rather than retry.
    async fn abandon_claim_before_admission_exact(
        &self,
        requirement_id: &str,
        owner_conversation_id: Option<&str>,
        owner_terminal_id: Option<&str>,
        expected_generation: i64,
        expected_claim_token: &str,
        now: TimestampMs,
    ) -> Result<Option<RequirementRow>, DbError>;
}

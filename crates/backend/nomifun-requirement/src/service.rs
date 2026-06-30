use std::sync::Arc;

use nomifun_api_types::{
    AttachmentDto, AutoWorkRunState, AutoWorkTargetKind, BoardResponse, CreateRequirementRequest,
    ListRequirementsQuery, Requirement, RequirementStatus, TagBinding, TagBindings, TagSummary,
    UpdateRequirementRequest,
};
use nomifun_common::{AppError, PaginatedResult, now_ms};
use nomifun_db::models::{RequirementRow, RequirementRowUpdate, RequirementTagRow};
use nomifun_db::{
    ConversationFilters, IConversationRepository, IRequirementRepository, ITerminalRepository, ListRequirementsParams,
};
use nomifun_terminal::TerminalDriver;
use tracing::warn;

use crate::attachments::AttachmentStore;
use crate::convert::row_to_dto;
use crate::events::RequirementEventEmitter;
use crate::notifier::CompletionNotifier;
use crate::order_key::to_sort_seq;

/// Default claim lease (ms). The orchestrator renews well within this window.
pub const DEFAULT_LEASE_MS: i64 = 120_000;
/// Max claim attempts before a requirement is left `failed` (poison-pill guard).
pub const MAX_ATTEMPTS: i64 = 3;

/// Parse an AutoWork/IDMM string `target_id` (a `conversation`/`terminal`
/// session id) into the integer key the repos/driver now use. The AutoWork DTO
/// carries `target_id` as a string (the generic, kind-agnostic target handle);
/// it is parsed to `i64` only at the repo/driver boundary. A non-numeric id —
/// e.g. a stale id replayed from a persisted ACP transcript (spec §2.5/§7.4) —
/// yields an explicit `NotFound` rather than silently pointing at another row.
fn parse_target_id(target_id: &str) -> Result<i64, AppError> {
    target_id
        .parse::<i64>()
        .map_err(|_| AppError::NotFound(format!("session {target_id}")))
}

/// Business logic for requirements (CRUD + AutoWork claim/finalize/config).
#[derive(Clone)]
pub struct RequirementService {
    repo: Arc<dyn IRequirementRepository>,
    emitter: RequirementEventEmitter,
    /// Attached for AutoWork config persistence (`extra.autowork` merge-write).
    conversation_service: Option<nomifun_conversation::ConversationService>,
    /// Attached for reading a conversation row when loading AutoWork config.
    conversation_repo: Option<Arc<dyn IConversationRepository>>,
    /// Attached for terminal AutoWork config + ownership/eligibility checks.
    terminal_driver: Option<Arc<dyn TerminalDriver>>,
    /// Attached to enumerate terminal sessions for the AutoWork admin
    /// (`tag_bindings`). The driver can describe a single terminal but cannot
    /// list them, so the repo is needed for the enumeration.
    terminal_repo: Option<Arc<dyn ITerminalRepository>>,
    /// Fired (detached) after a requirement reaches a terminal state, so a bound
    /// webhook can notify. Optional + non-blocking — a failing webhook never
    /// affects requirement state.
    completion_notifier: Option<Arc<dyn CompletionNotifier>>,
    /// Notified whenever a requirement becomes claimable (created or re-pended),
    /// so idle AutoWork loops wake immediately instead of waiting for their poll
    /// fallback. Attached during assembly to the same `Notify` the orchestrator
    /// loops await on. `None` on instances that never drive AutoWork (the sink).
    autowork_waker: Option<Arc<tokio::sync::Notify>>,
    /// Attached for persistent image attachments (bind/copy/delete + AutoWork
    /// workspace staging). `None` on instances that never touch attachments
    /// (e.g. the declaration sink).
    attachments: Option<Arc<AttachmentStore>>,
}

impl RequirementService {
    pub fn new(repo: Arc<dyn IRequirementRepository>, emitter: RequirementEventEmitter) -> Self {
        Self {
            repo,
            emitter,
            conversation_service: None,
            conversation_repo: None,
            terminal_driver: None,
            terminal_repo: None,
            completion_notifier: None,
            autowork_waker: None,
            attachments: None,
        }
    }

    /// Attach the conversation service + repo for AutoWork config persistence.
    pub fn with_conversation_service(
        mut self,
        cs: nomifun_conversation::ConversationService,
        conv_repo: Arc<dyn IConversationRepository>,
    ) -> Self {
        self.conversation_service = Some(cs);
        self.conversation_repo = Some(conv_repo);
        self
    }

    /// Attach the terminal driver for terminal AutoWork config + ownership.
    pub fn with_terminal_driver(mut self, driver: Arc<dyn TerminalDriver>) -> Self {
        self.terminal_driver = Some(driver);
        self
    }

    /// Attach the terminal repo to enumerate terminal AutoWork bindings.
    pub fn with_terminal_repo(mut self, repo: Arc<dyn ITerminalRepository>) -> Self {
        self.terminal_repo = Some(repo);
        self
    }

    /// Attach only the conversation repo (without the full conversation service).
    /// `with_conversation_service` also sets it; this is for callers/tests that
    /// need just the read side (e.g. `tag_bindings`).
    pub fn with_conversation_repo(mut self, repo: Arc<dyn IConversationRepository>) -> Self {
        self.conversation_repo = Some(repo);
        self
    }

    /// Attach the completion notifier fired on terminal status transitions.
    pub fn with_completion_notifier(mut self, notifier: Arc<dyn CompletionNotifier>) -> Self {
        self.completion_notifier = Some(notifier);
        self
    }

    /// Attach the AutoWork waker. Shared with the orchestrator: transitions that
    /// make a requirement claimable (`create`, re-pend) notify it so idle loops
    /// pick up new work without waiting for their poll fallback.
    pub fn with_autowork_waker(mut self, waker: Arc<tokio::sync::Notify>) -> Self {
        self.autowork_waker = Some(waker);
        self
    }

    /// Attach the attachment store (persistent requirement images).
    pub fn with_attachment_store(mut self, store: Arc<AttachmentStore>) -> Self {
        self.attachments = Some(store);
        self
    }

    /// Attachments of a requirement as DTOs; empty when no store is attached
    /// or on a read failure (display data must not fail the main call).
    async fn load_attachments(&self, requirement_id: i64) -> Vec<AttachmentDto> {
        let Some(store) = &self.attachments else { return Vec::new() };
        match store.list(requirement_id).await {
            Ok(rows) => rows.iter().map(|r| store.to_dto(r)).collect(),
            Err(e) => {
                warn!(error = %e, requirement_id, "failed to load requirement attachments");
                Vec::new()
            }
        }
    }

    /// Staging entry point for the orchestrator: copy the requirement's
    /// attachments into the session workspace (when given) and return prompt
    /// entries. Empty when no store is attached.
    pub async fn stage_attachments_for_prompt(
        &self,
        req_id: i64,
        workspace: Option<&std::path::Path>,
    ) -> Vec<crate::attachments::PromptAttachment> {
        match &self.attachments {
            Some(store) => store.stage_for_prompt(req_id, workspace).await,
            None => Vec::new(),
        }
    }

    /// Wake idle AutoWork loops (no-op when no waker is attached). Called after a
    /// requirement becomes `pending` so a bound-but-idle session claims it now.
    fn wake_autowork(&self) {
        if let Some(waker) = &self.autowork_waker {
            waker.notify_waiters();
        }
    }

    /// Expose the repo for the orchestrator / sweeper (Phase C).
    pub fn repo(&self) -> &Arc<dyn IRequirementRepository> {
        &self.repo
    }

    pub async fn create(&self, mut req: CreateRequirementRequest) -> Result<Requirement, AppError> {
        let new_attachments = std::mem::take(&mut req.attachments);
        if req.title.trim().is_empty() {
            return Err(AppError::BadRequest("title must not be empty".into()));
        }
        if req.tag.trim().is_empty() {
            return Err(AppError::BadRequest("tag must not be empty".into()));
        }
        let now = now_ms();
        let order_key = req.order_key.unwrap_or_default();
        let status = req.status.unwrap_or(RequirementStatus::Pending);
        let row = RequirementRow {
            // Placeholder id — the DB mints the real INTEGER PRIMARY KEY on
            // insert and returns it via `last_insert_rowid()`. The id field on
            // the in-struct row is ignored by `insert`.
            id: 0,
            title: req.title,
            content: req.content,
            tag: req.tag,
            sort_seq: to_sort_seq(&order_key),
            order_key,
            status: status.as_db().to_string(),
            // `priority` column is retained in the DB for compatibility but is no
            // longer user-facing — `order_key` is the sole ordering dimension.
            priority: 0,
            completion_note: None,
            owner_session_id: None,
            owner_kind: None,
            claimed_at: None,
            lease_expires_at: None,
            started_at: None,
            completed_at: None,
            attempt_count: 0,
            created_by: req.created_by.unwrap_or_else(|| "user".to_string()),
            extra: "{}".to_string(),
            created_at: now,
            updated_at: now,
        };
        // Back-end minted id: the repo returns the real `last_insert_rowid()`.
        let new_id = self.repo.insert(&row).await?;
        let row = RequirementRow { id: new_id, ..row };
        let mut dto = row_to_dto(&row);
        if !new_attachments.is_empty() {
            let Some(store) = &self.attachments else {
                let _ = self.repo.delete(row.id).await;
                return Err(AppError::Internal("attachment store not attached".into()));
            };
            match store.ingest(row.id, &new_attachments, Some(&row.created_by)).await {
                Ok(rows) => dto.attachments = rows.iter().map(|r| store.to_dto(r)).collect(),
                Err(e) => {
                    // Keep create atomic for the caller: drop the row we just inserted.
                    if let Err(de) = self.repo.delete(row.id).await {
                        warn!(error = %de, requirement_id = row.id, "rollback after attachment ingest failure failed");
                    }
                    return Err(e);
                }
            }
        }
        self.emitter.emit_created(&dto);
        // A freshly-created pending requirement is claimable now — wake idle loops.
        if dto.status == RequirementStatus::Pending {
            self.wake_autowork();
        }
        Ok(dto)
    }

    pub async fn get(&self, id: i64) -> Result<Requirement, AppError> {
        let row = self
            .repo
            .get_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("requirement {id}")))?;
        let mut dto = row_to_dto(&row);
        dto.attachments = self.load_attachments(id).await;
        Ok(dto)
    }

    pub async fn list(&self, query: &ListRequirementsQuery) -> Result<PaginatedResult<Requirement>, AppError> {
        let page = query.page.unwrap_or(1).max(1);
        let page_size = query.page_size.unwrap_or(20).clamp(1, 200);
        let params = ListRequirementsParams {
            tag: query.tag.clone(),
            status: query.status.map(|s| s.as_db().to_string()),
            owner_session_id: query.conversation_id,
            // The public list query has no kind filter — `conversation_id` here
            // is a UI filter that historically meant the conversation domain.
            owner_kind: None,
            q: query.q.clone(),
            order_by: query.order_by.clone(),
            order: query.order.clone(),
            page: Some(page),
            page_size: Some(page_size),
        };
        let (rows, total) = self.repo.list(&params).await?;
        let items: Vec<Requirement> = rows.iter().map(row_to_dto).collect();
        let has_more = (page as u64) * (page_size as u64) < total;
        Ok(PaginatedResult { items, total, has_more })
    }

    pub async fn update(&self, id: i64, req: UpdateRequirementRequest) -> Result<Requirement, AppError> {
        // Ensure it exists for a clean 404 (update() also returns NotFound).
        let row = self
            .repo
            .get_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("requirement {id}")))?;

        // Attachment changes first — ingest BEFORE remove. Ingest is the only
        // high-failure-probability step (validation, the temp source may already
        // be cleaned) and is all-or-nothing, so a failure here leaves the row and
        // its existing attachments completely untouched. Remove afterwards only
        // deletes DB rows + best-effort files and practically cannot fail; in the
        // extreme case it does, the freshly-ingested attachments are kept and a
        // retry of the same update converges (remove skips already-gone ids).
        let attachments_changed = !req.remove_attachment_ids.is_empty() || !req.add_attachments.is_empty();
        if attachments_changed {
            let Some(store) = &self.attachments else {
                return Err(AppError::Internal("attachment store not attached".into()));
            };
            store.ingest(id, &req.add_attachments, None).await?;
            store.remove(id, &req.remove_attachment_ids).await?;
        }

        let mut params = RequirementRowUpdate {
            title: req.title,
            content: req.content,
            tag: req.tag,
            status: req.status.map(|s| s.as_db().to_string()),
            completion_note: req.completion_note.map(Some),
            ..Default::default()
        };
        if let Some(ok) = req.order_key {
            params.sort_seq = Some(to_sort_seq(&ok));
            params.order_key = Some(ok);
        }
        // Attachment-only update: every row field is None, so repo.update would
        // early-return on its empty SET list and leave updated_at stale while we
        // still emit `requirement.updated`. Force the SQL path with an equal-value
        // field — repo.update stamps updated_at itself.
        if attachments_changed
            && params.title.is_none()
            && params.content.is_none()
            && params.tag.is_none()
            && params.status.is_none()
            && params.completion_note.is_none()
            && params.order_key.is_none()
        {
            params.status = Some(row.status.clone());
        }
        self.repo.update(id, &params).await?;

        let row = self
            .repo
            .get_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("requirement {id}")))?;
        let mut dto = row_to_dto(&row);
        dto.attachments = self.load_attachments(id).await;
        self.emitter.emit_updated(&dto);
        Ok(dto)
    }

    pub async fn delete(&self, id: i64) -> Result<(), AppError> {
        // Clean attachment files+rows BEFORE deleting the requirement: the
        // `attachments.requirement_id` FK is `ON DELETE CASCADE`, so deleting the
        // row first would cascade-drop the attachment rows and leave `delete_all`
        // (which lists rows to find their files) nothing to remove — orphaning the
        // files on disk. File failures are logged, not raised.
        if let Some(store) = &self.attachments
            && let Err(e) = store.delete_all(id).await
        {
            warn!(error = %e, requirement_id = id, "attachment cleanup failed on requirement delete");
        }
        self.repo.delete(id).await?;
        self.emitter.emit_deleted(id);
        Ok(())
    }

    /// Delete many requirements by id. Missing ids are skipped (not an error).
    /// Returns the number actually deleted; emits `requirement.deleted` per row.
    pub async fn delete_many(&self, ids: &[i64]) -> Result<u64, AppError> {
        let mut deleted = 0u64;
        for &id in ids {
            // Files first — the requirement_id FK cascades and would otherwise
            // drop the attachment rows before delete_all can find their files.
            if let Some(store) = &self.attachments
                && let Err(e) = store.delete_all(id).await
            {
                warn!(error = %e, requirement_id = id, "attachment cleanup failed on requirement delete");
            }
            match self.repo.delete(id).await {
                Ok(()) => {
                    self.emitter.emit_deleted(id);
                    deleted += 1;
                }
                Err(nomifun_db::DbError::NotFound(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(deleted)
    }

    pub async fn tags(&self) -> Result<Vec<TagSummary>, AppError> {
        let counts = self.repo.tag_status_counts().await?;
        let mut summaries: Vec<TagSummary> = Vec::new();
        for (tag, status, count) in counts {
            let entry = match summaries.iter_mut().find(|s| s.tag == tag) {
                Some(e) => e,
                None => {
                    summaries.push(TagSummary {
                        tag: tag.clone(),
                        ..Default::default()
                    });
                    summaries.last_mut().unwrap()
                }
            };
            match status.as_str() {
                "pending" => entry.pending += count,
                "in_progress" => entry.in_progress += count,
                "done" => entry.done += count,
                "failed" => entry.failed += count,
                "cancelled" => entry.cancelled += count,
                "needs_review" => entry.needs_review += count,
                _ => {}
            }
            entry.total += count;
        }
        // Annotate AutoWork pause state per tag (tag count is small).
        for summary in &mut summaries {
            if let Some(st) = self.repo.get_tag_state(&summary.tag).await? {
                summary.paused = st.is_paused();
                summary.paused_reason = if st.is_paused() { st.paused_reason } else { None };
            }
        }
        Ok(summaries)
    }

    pub async fn board(&self, tag: &str) -> Result<BoardResponse, AppError> {
        let rows = self.repo.list_by_tag(tag).await?;
        let mut board = BoardResponse {
            tag: tag.to_string(),
            pending: Vec::new(),
            in_progress: Vec::new(),
            done: Vec::new(),
            failed: Vec::new(),
            cancelled: Vec::new(),
            needs_review: Vec::new(),
        };
        for row in &rows {
            let dto = row_to_dto(row);
            match RequirementStatus::from_db(&row.status) {
                RequirementStatus::Pending => board.pending.push(dto),
                RequirementStatus::InProgress => board.in_progress.push(dto),
                RequirementStatus::Done => board.done.push(dto),
                RequirementStatus::Failed => board.failed.push(dto),
                RequirementStatus::Cancelled => board.cancelled.push(dto),
                RequirementStatus::NeedsReview => board.needs_review.push(dto),
            }
        }
        Ok(board)
    }

    /// Atomically claim the next pending requirement for `tag`, owned by
    /// `owner_session_id` (a `conv_*` or `term_*` id). `kind` discriminates the
    /// owner domain and is persisted as `owner_kind` (paired with the session id
    /// to satisfy the table's paired-NULL CHECK). Emits `requirement.statusChanged`
    /// for the claimed row.
    pub async fn claim_next(
        &self,
        tag: &str,
        owner_session_id: i64,
        kind: AutoWorkTargetKind,
        lease_ms: i64,
    ) -> Result<Option<Requirement>, AppError> {
        let claimed = self
            .repo
            .claim_next(tag, owner_session_id, kind.as_str(), lease_ms, now_ms())
            .await?;
        Ok(claimed.map(|row| {
            let dto = row_to_dto(&row);
            self.emitter.emit_status_changed(&dto);
            dto
        }))
    }

    /// Renew the lease for `id` held by `owner_session_id`. Returns whether a row matched.
    pub async fn renew_lease(&self, id: i64, owner_session_id: i64, lease_ms: i64) -> Result<bool, AppError> {
        Ok(self.repo.renew_lease(id, owner_session_id, lease_ms, now_ms()).await?)
    }

    /// Verify `conversation_id` belongs to `user_id` (data isolation for the
    /// claim / autowork routes). No-op when no conversation repo is attached
    /// (e.g. the sink-only service instance) or for legacy rows with an empty
    /// owner. Returns `NotFound` if the conversation does not exist, `Forbidden`
    /// if owned by another user.
    pub async fn verify_conversation_owner(&self, conversation_id: i64, user_id: &str) -> Result<(), AppError> {
        let Some(conv_repo) = &self.conversation_repo else {
            return Ok(());
        };
        let row = conv_repo
            .get(conversation_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("conversation {conversation_id}")))?;
        if !row.user_id.is_empty() && row.user_id != user_id {
            return Err(AppError::Forbidden(format!(
                "conversation {conversation_id} is not owned by the caller"
            )));
        }
        Ok(())
    }

    /// stopped mid-turn). No-op unless the requirement is `in_progress` and held
    /// by `conversation_id` IN THE CONVERSATION DOMAIN. Does NOT consume
    /// `attempt_count` — a user stop is not a failed attempt. Emits
    /// `requirement.statusChanged`.
    ///
    /// SECURITY (C2, spec §2.2): `owner_session_id` is dual-domain
    /// (conversation|terminal); after integerization `conv#5` and `term#5` share
    /// the same numeric value. The owner comparison is therefore **paired with
    /// `owner_kind`** — a conversation caller never releases a terminal-owned
    /// requirement that merely shares its number.
    pub async fn release_claim(&self, id: i64, conversation_id: i64) -> Result<(), AppError> {
        let Some(row) = self.repo.get_by_id(id).await? else {
            return Ok(());
        };
        if row.status != "in_progress"
            || row.owner_kind.as_deref() != Some(AutoWorkTargetKind::Conversation.as_str())
            || row.owner_session_id != Some(conversation_id)
        {
            return Ok(());
        }
        let params = RequirementRowUpdate {
            status: Some("pending".to_string()),
            owner_session_id: Some(None),
            owner_kind: Some(None),
            claimed_at: Some(None),
            lease_expires_at: Some(None),
            ..Default::default()
        };
        self.repo.update(id, &params).await?;
        if let Some(updated) = self.repo.get_by_id(id).await? {
            self.emitter.emit_status_changed(&row_to_dto(&updated));
        }
        // Released back to pending → another bound session may claim it now.
        self.wake_autowork();
        Ok(())
    }

    /// The user manually cancelled an AutoWork-driven turn — treat it as an
    /// explicit "stop working on this" signal, NOT a failed attempt:
    /// 1. pause the tag (reason `user_interrupted`, resumable from the UI) so
    ///    the persistent loop does not immediately re-claim and re-inject the
    ///    same requirement — the historical "I paused it and seconds later it
    ///    was running again";
    /// 2. release the claim back to `pending` WITHOUT consuming an attempt.
    /// Ordered pause-first so the release's wake cannot race a re-claim (the
    /// claim SQL skips paused tags). Best-effort on the pause write: a failure
    /// must not block the claim release.
    pub async fn user_interrupt(&self, id: i64, conversation_id: i64, tag: &str) -> Result<(), AppError> {
        match self.repo.pause_tag(tag, "user_interrupted", Some(id), now_ms()).await {
            Ok(()) => self.emitter.emit_tag_paused(&nomifun_api_types::TagPausedPayload {
                tag: tag.to_string(),
                reason: "user_interrupted".to_string(),
                requirement_id: Some(id),
            }),
            Err(e) => warn!(
                tag,
                requirement_id = id,
                error = %e,
                "Failed to pause tag after user interrupt"
            ),
        }
        self.release_claim(id, conversation_id).await
    }

    /// Agent/user self-update: set status, with timestamps. `done` sets
    /// `completed_at` (+ optional note); `failed` records the note. Idempotent on
    /// any terminal state (re-setting the same status is a no-op). Rejects
    /// transitions out of a terminal state (`done`/`failed`/`cancelled`).
    pub async fn set_status(
        &self,
        id: i64,
        status: RequirementStatus,
        note: Option<String>,
    ) -> Result<Requirement, AppError> {
        let row = self
            .repo
            .get_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("requirement {id}")))?;

        // Idempotent: re-setting the current status is a no-op (covers done->done,
        // failed->failed, cancelled->cancelled, and avoids a duplicate WS event).
        if row.status == status.as_db() {
            return Ok(row_to_dto(&row));
        }

        // A terminal requirement is frozen: reject transitions out of done/failed/
        // cancelled. (Re-running a requirement means creating a new one.)
        if matches!(row.status.as_str(), "done" | "failed" | "cancelled") {
            return Err(AppError::BadRequest(format!(
                "requirement {id} is {} and cannot transition to {}",
                row.status,
                status.as_db()
            )));
        }

        let now = now_ms();
        let mut params = RequirementRowUpdate {
            status: Some(status.as_db().to_string()),
            ..Default::default()
        };
        match status {
            RequirementStatus::Done => {
                params.completed_at = Some(Some(now));
                // A verdict always (re)writes the completion note: a fresh note
                // overwrites, and NO note clears any stale prose left by a prior
                // attempt (e.g. an apology parked as needs_review, then re-pended and
                // completed by the terminal path which passes note=None). Without this
                // the old note lingers on a done requirement and misleads.
                params.completion_note = Some(note);
            }
            RequirementStatus::Failed => {
                params.completion_note = Some(note);
            }
            RequirementStatus::InProgress => {
                params.started_at = Some(Some(row.started_at.unwrap_or(now)));
            }
            RequirementStatus::NeedsReview => {
                // Keep the agent's prose as the note so the reviewing human sees what
                // the turn produced; a verdict with no note clears any stale prose.
                params.completion_note = Some(note);
            }
            _ => {}
        }
        self.repo.update(id, &params).await?;

        let updated = self
            .repo
            .get_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("requirement {id}")))?;
        let dto = row_to_dto(&updated);
        self.emitter.emit_status_changed(&dto);

        // Fire the completion notifier on terminal transitions. The early-returns
        // above guarantee this is a genuine change out of a non-terminal state, so
        // this runs at most once per requirement. Detached + best-effort: a slow or
        // failing webhook must never block or fail the status transition.
        // `NeedsReview` is included because it is exactly a "human, please look"
        // signal worth notifying on, even though it is not a frozen terminal state.
        if matches!(
            status,
            RequirementStatus::Done | RequirementStatus::Failed | RequirementStatus::NeedsReview
        ) && let Some(notifier) = &self.completion_notifier
        {
            let notifier = notifier.clone();
            let row = updated.clone();
            tokio::spawn(async move {
                notifier.notify_completion(&row).await;
            });
        }
        Ok(dto)
    }

    /// Convenience: mark done with a completion note.
    pub async fn complete(&self, id: i64, completion_note: Option<String>) -> Result<Requirement, AppError> {
        self.set_status(id, RequirementStatus::Done, completion_note).await
    }

    /// Broadcast an AutoWork state change (used by the routes layer).
    pub fn emit_autowork_state(&self, state: &nomifun_api_types::AutoWorkState) {
        self.emitter.emit_autowork_changed(state);
    }

    /// Persist the AutoWork config `{ enabled, tag, max_requirements }` for a
    /// target. Conversations store it under `extra.autowork`; terminals store it
    /// in the `terminal_sessions.autowork` JSON column (via the driver).
    pub async fn save_autowork_config(
        &self,
        kind: AutoWorkTargetKind,
        target_id: &str,
        enabled: bool,
        tag: Option<&str>,
        max_requirements: Option<u32>,
    ) -> Result<(), AppError> {
        match kind {
            AutoWorkTargetKind::Conversation => {
                let Some(cs) = &self.conversation_service else {
                    return Err(AppError::Internal("conversation service not attached".into()));
                };
                cs.update_extra(
                    target_id,
                    serde_json::json!({
                        "autowork": {
                            "enabled": enabled,
                            "tag": tag,
                            "max_requirements": max_requirements,
                        }
                    }),
                )
                .await
            }
            AutoWorkTargetKind::Terminal => {
                let Some(driver) = &self.terminal_driver else {
                    return Err(AppError::Internal("terminal driver not attached".into()));
                };
                let blob = serde_json::json!({
                    "enabled": enabled,
                    "tag": tag,
                    "max_requirements": max_requirements,
                })
                .to_string();
                driver.write_autowork(parse_target_id(target_id)?, Some(&blob)).await?;
                Ok(())
            }
        }
    }

    /// Read the persisted AutoWork config `(enabled, tag, max)` for a target.
    /// Returns `(false, None, None)` when no backing store is attached or no
    /// config exists. For conversations, falls back to the legacy `autopilot`
    /// key once so a config written before the rename is not orphaned.
    pub async fn read_autowork_config(
        &self,
        kind: AutoWorkTargetKind,
        target_id: &str,
    ) -> Result<(bool, Option<String>, Option<u32>), AppError> {
        let raw: Option<serde_json::Value> = match kind {
            AutoWorkTargetKind::Conversation => {
                let Some(conv_repo) = &self.conversation_repo else {
                    return Ok((false, None, None));
                };
                let Some(row) = conv_repo.get(parse_target_id(target_id)?).await? else {
                    return Ok((false, None, None));
                };
                let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_default();
                extra
                    .get("autowork")
                    .or_else(|| extra.get("autopilot")) // legacy read-compat (pre-rename)
                    .cloned()
            }
            AutoWorkTargetKind::Terminal => {
                let Some(driver) = &self.terminal_driver else {
                    return Ok((false, None, None));
                };
                match driver.read_autowork(parse_target_id(target_id)?).await? {
                    Some(s) => serde_json::from_str(&s).ok(),
                    None => None,
                }
            }
        };
        let aw = raw.unwrap_or_default();
        let enabled = aw.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
        let tag = aw.get("tag").and_then(|v| v.as_str()).map(|s| s.to_string());
        let max = aw.get("max_requirements").and_then(|v| v.as_u64()).map(|n| n as u32);
        Ok((enabled, tag, max))
    }

    /// Verify `terminal_id` belongs to `user_id` (data isolation for the terminal
    /// AutoWork routes). No-op when no terminal driver is attached. `NotFound` if
    /// the terminal does not exist, `Forbidden` if owned by someone else.
    pub async fn verify_terminal_owner(&self, terminal_id: &str, user_id: &str) -> Result<(), AppError> {
        let Some(driver) = &self.terminal_driver else {
            return Ok(());
        };
        let desc = driver
            .describe(parse_target_id(terminal_id)?)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("terminal {terminal_id}")))?;
        if desc.user_id != user_id {
            return Err(AppError::Forbidden(format!(
                "terminal {terminal_id} is not owned by the caller"
            )));
        }
        Ok(())
    }

    /// Ensure a terminal is eligible for AutoWork: it must be a verdict-capable
    /// agent CLI (one with a lifecycle-hook renderer — claude/codex, including
    /// wrappers like `stepcode claude` — those get the Stop → TurnEnd hook +
    /// requirement MCP injected) and currently running. `BadRequest` otherwise.
    ///
    /// Eligibility is resolved from the launch `(command, args, backend)` via
    /// `nomifun_terminal::terminal_autowork_capable`, the SAME logic the launch
    /// injector uses — so the gate never rejects a terminal the platform would
    /// actually hook (the historical bug: a custom/wrapper launch stored
    /// `backend = None` and was rejected despite being injectable).
    pub async fn ensure_terminal_autowork_eligible(&self, terminal_id: &str) -> Result<(), AppError> {
        let Some(driver) = &self.terminal_driver else {
            return Err(AppError::Internal("terminal driver not attached".into()));
        };
        let desc = driver
            .describe(parse_target_id(terminal_id)?)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("terminal {terminal_id}")))?;
        let is_agent =
            nomifun_terminal::terminal_autowork_capable(&desc.command, &desc.args, desc.backend.as_deref());
        if !is_agent {
            return Err(AppError::BadRequest(
                "AutoWork requires an agent CLI terminal with lifecycle hooks (claude / codex, including wrappers like `stepcode claude`)".into(),
            ));
        }
        if desc.last_status != "running" {
            return Err(AppError::BadRequest("terminal is not running".into()));
        }
        Ok(())
    }

    /// Called by the orchestrator after a turn ends. If the agent already moved
    /// the row to a terminal state (via its completion tool / terminal marker),
    /// respect it. Otherwise:
    /// - clean turn + `expects_verdict` → mark `needs_review` (the agent had a
    ///   way to declare done/failed but didn't, so we do NOT silently assume
    ///   success — a human verifies). This is the soft-failure guard.
    /// - clean turn + NOT `expects_verdict` → mark `done` (legacy: the engine has
    ///   no declaration channel, so a clean finish is the best signal we have).
    /// - error → if `attempt_count < MAX_ATTEMPTS` re-pend for retry, else mark
    ///   `failed` and pause the tag.
    ///
    /// `expects_verdict` is true when the engine WAS given an explicit way to
    /// declare the outcome (nomi native tools, ACP requirement MCP, terminal
    /// marker). Returns the final DTO (or None if the row vanished).
    pub async fn finalize_if_needed(
        &self,
        id: i64,
        turn_errored: bool,
        note: Option<String>,
        expects_verdict: bool,
    ) -> Result<Option<Requirement>, AppError> {
        let Some(row) = self.repo.get_by_id(id).await? else {
            return Ok(None);
        };
        // Agent already reached a terminal state itself → respect it (its own
        // note, e.g. from the nomi `requirement_complete` tool, wins).
        if row.status == "done" || row.status == "failed" || row.status == "cancelled" {
            return Ok(Some(row_to_dto(&row)));
        }
        // Still in_progress (or pending): decide based on the turn outcome.
        if !turn_errored {
            let note = note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
            if expects_verdict {
                // The agent could have declared done/failed but ended the turn
                // without doing so → ambiguous. Park for human review instead of
                // silently recording success.
                let dto = self.set_status(id, RequirementStatus::NeedsReview, note).await?;
                return Ok(Some(dto));
            }
            // Tool-free engine with no declaration channel: a clean finish is the
            // completion signal; capture the agent's prose as the note.
            let dto = self.set_status(id, RequirementStatus::Done, note).await?;
            return Ok(Some(dto));
        }
        // Errored turn.
        if row.attempt_count < MAX_ATTEMPTS {
            // Re-pend for another attempt: clear the claim. `repo.update` stamps
            // `updated_at` itself, so no timestamp is needed here.
            let params = RequirementRowUpdate {
                status: Some("pending".to_string()),
                owner_session_id: Some(None),
                owner_kind: Some(None),
                claimed_at: Some(None),
                lease_expires_at: Some(None),
                ..Default::default()
            };
            self.repo.update(id, &params).await?;
            let updated = self.repo.get_by_id(id).await?;
            // Back to pending → wake idle loops (this or a sibling session retries).
            self.wake_autowork();
            return Ok(updated.map(|r| {
                let dto = row_to_dto(&r);
                self.emitter.emit_status_changed(&dto);
                dto
            }));
        }
        let dto = self
            .set_status(id, RequirementStatus::Failed, Some("exhausted retries".into()))
            .await?;
        // A requirement exhausted its retries → PAUSE the whole tag so the
        // orchestrator stops claiming the tag's remaining requirements until a
        // human resumes it. This is the fix for "a failed predecessor lets every
        // successor barge in instantly". Best-effort: a pause-write failure must
        // not mask the failed-status transition that already succeeded.
        match self.repo.pause_tag(&row.tag, "requirement_failed", Some(id), now_ms()).await {
            Ok(()) => self.emitter.emit_tag_paused(&nomifun_api_types::TagPausedPayload {
                tag: row.tag.clone(),
                reason: "requirement_failed".to_string(),
                requirement_id: Some(id),
            }),
            Err(e) => warn!(
                tag = %row.tag,
                requirement_id = id,
                error = %e,
                "Failed to pause tag after requirement exhaustion"
            ),
        }
        Ok(Some(dto))
    }

    /// Whether `tag` is currently paused (AutoWork halted for it).
    pub async fn is_tag_paused(&self, tag: &str) -> Result<bool, AppError> {
        Ok(self.repo.is_tag_paused(tag).await?)
    }

    /// Full pause state for a tag (`None` = never paused).
    pub async fn tag_state(&self, tag: &str) -> Result<Option<RequirementTagRow>, AppError> {
        Ok(self.repo.get_tag_state(tag).await?)
    }

    /// Resume a paused tag. Optionally re-queue specific failed requirements back
    /// to `pending` (clearing their consumed attempts) so they retry from
    /// scratch. Wakes idle AutoWork loops so the tag's work resumes immediately.
    pub async fn resume_tag(&self, tag: &str, requeue_ids: &[i64]) -> Result<(), AppError> {
        self.repo.resume_tag(tag).await?;
        for &id in requeue_ids {
            // Only re-pend rows that are genuinely failed AND belong to this tag.
            let Some(row) = self.repo.get_by_id(id).await? else { continue };
            if row.tag != tag || row.status != "failed" {
                continue;
            }
            let params = RequirementRowUpdate {
                status: Some("pending".to_string()),
                completion_note: Some(None),
                owner_session_id: Some(None),
                owner_kind: Some(None),
                claimed_at: Some(None),
                lease_expires_at: Some(None),
                attempt_count: Some(0),
                ..Default::default()
            };
            self.repo.update(id, &params).await?;
            if let Some(updated) = self.repo.get_by_id(id).await? {
                self.emitter.emit_status_changed(&row_to_dto(&updated));
            }
        }
        // Tag is active again (+ any requeued rows are pending) → wake idle loops.
        self.wake_autowork();
        Ok(())
    }

    /// Resume a tag because AutoWork was explicitly (re-)ENABLED on a session
    /// bound to it. A paused tag (prior `requirement_failed`, or a deleted-session
    /// cascade) otherwise silently blocks EVERY conversation bound to the same tag
    /// — the user toggles 自动工作 on and nothing happens, with no per-conversation
    /// indication that the shared tag is paused (the recurring "彻底不工作" trap).
    ///
    /// An explicit enable is a clear "run this" signal, so: unpause the tag and
    /// give its STUCK requirements (`failed` / `pending` / stale `in_progress`) a
    /// fresh attempt budget (re-pend, clear owner, reset attempt_count). Rows the
    /// user deliberately parked for review (`needs_review`) and terminal rows
    /// (`done` / `cancelled`) are left untouched. No-op when the tag is not paused.
    pub async fn resume_tag_for_enable(&self, tag: &str) -> Result<(), AppError> {
        if !self.repo.is_tag_paused(tag).await? {
            return Ok(());
        }
        self.repo.resume_tag(tag).await?;
        for row in self.repo.list_by_tag(tag).await? {
            if !matches!(row.status.as_str(), "failed" | "pending" | "in_progress") {
                continue;
            }
            let params = RequirementRowUpdate {
                status: Some("pending".to_string()),
                completion_note: Some(None),
                owner_session_id: Some(None),
                owner_kind: Some(None),
                claimed_at: Some(None),
                lease_expires_at: Some(None),
                attempt_count: Some(0),
                ..Default::default()
            };
            self.repo.update(row.id, &params).await?;
            if let Some(updated) = self.repo.get_by_id(row.id).await? {
                self.emitter.emit_status_changed(&row_to_dto(&updated));
            }
        }
        self.wake_autowork();
        Ok(())
    }


    /// WITHOUT consuming an attempt (the turn never ran). Wakes loops to retry.
    pub async fn unclaim_busy(&self, id: i64, conversation_id: i64) -> Result<(), AppError> {
        if self.repo.unclaim(id, conversation_id).await? {
            if let Some(updated) = self.repo.get_by_id(id).await? {
                self.emitter.emit_status_changed(&row_to_dto(&updated));
            }
            self.wake_autowork();
        }
        Ok(())
    }

    /// §9.B — clear the owner of every requirement bound to a now-deleted
    /// session (`owner_session_id` matches), unblocking requirements whose
    /// `conv_*`/`term_*` executing session was deleted.
    ///
    /// `owner_session_id` is a dual-domain column with NO FK (a single column
    /// cannot reference two parent tables), so a deleted conversation/terminal
    /// does NOT cascade-clear it. Without this hook a requirement claimed by a
    /// since-deleted session would keep a dangling owner and, if it was
    /// `in_progress`, sit orphaned until the lease sweeper happened to run.
    ///
    /// Clears both owner columns together (paired-NULL CHECK), and re-pends any
    /// `in_progress` row (its session is gone, so it can never finish) WITHOUT
    /// consuming an attempt — the session vanishing is not a failed attempt.
    /// Idempotent; wakes idle loops so a re-pended requirement is reclaimable now.
    ///
    /// CALL SITE (Phase 3/4 wiring): invoke from the conversations + terminal
    /// deletion paths (`nomifun-conversation` / `nomifun-terminal` delete) with
    /// the deleted session id AND its domain. Exposed here so the deletion path
    /// can call it without the DB layer (the FK that would have cascaded does
    /// not exist).
    ///
    /// SECURITY (spec §2.2): the query is scoped by BOTH `owner_session_id` and
    /// `owner_kind` — after integerization a conversation and a terminal can
    /// share a numeric id, so a kind-less scan would clear the OTHER domain's
    /// requirements that merely share the number.
    pub async fn clear_owner_for_session(
        &self,
        session_id: i64,
        kind: AutoWorkTargetKind,
    ) -> Result<u64, AppError> {
        // Page through every requirement owned by this session (paired domain).
        let mut cleared = 0u64;
        let mut woke = false;
        loop {
            let params = ListRequirementsParams {
                owner_session_id: Some(session_id),
                owner_kind: Some(kind.as_str().to_string()),
                page: Some(1),
                page_size: Some(200),
                ..Default::default()
            };
            let (rows, _total) = self.repo.list(&params).await?;
            if rows.is_empty() {
                break;
            }
            let batch = rows.len();
            for row in &rows {
                let re_pend = row.status == "in_progress";
                let mut update = RequirementRowUpdate {
                    owner_session_id: Some(None),
                    owner_kind: Some(None),
                    ..Default::default()
                };
                if re_pend {
                    update.status = Some("pending".to_string());
                    update.claimed_at = Some(None);
                    update.lease_expires_at = Some(None);
                    woke = true;
                }
                self.repo.update(row.id, &update).await?;
                if let Some(updated) = self.repo.get_by_id(row.id).await? {
                    self.emitter.emit_status_changed(&row_to_dto(&updated));
                }
                cleared += 1;
            }
            // The list query filters on owner_session_id; cleared rows drop out of
            // the result set, so re-querying page 1 always advances. Guard against
            // a short page (no more rows) to terminate.
            if batch < 200 {
                break;
            }
        }
        if woke {
            self.wake_autowork();
        }
        Ok(cleared)
    }

    /// Enumerate AutoWork tag→session bindings for `user_id`, grouped by tag.
    ///
    /// A "binding" is a conversation or terminal whose persisted AutoWork config
    /// is `enabled`, pointing at a tag. The `run_state` returned here reflects the
    /// persisted config only (`Idle` for every enabled binding); the routes layer
    /// upgrades it to `Active` for targets the orchestrator is currently driving
    /// (it owns the live progress map). Used by the AutoWork admin 标签会话管理 tab.
    pub async fn tag_bindings(&self, user_id: &str) -> Result<Vec<TagBindings>, AppError> {
        // (tag, binding) accumulator, grouped at the end.
        let mut by_tag: std::collections::BTreeMap<String, Vec<TagBinding>> = std::collections::BTreeMap::new();

        // Conversations: page through all of the user's conversations and parse
        // each `extra.autowork` (with legacy `autopilot` fallback) directly from
        // the row we already hold (no extra per-row query).
        if let Some(conv_repo) = &self.conversation_repo {
            let mut cursor: Option<i64> = None;
            loop {
                let filters = ConversationFilters {
                    cursor,
                    limit: 200,
                    source: None,
                    cron_job_id: None,
                    pinned: None,
                    // 需求采集扫描全部会话(含 companion);保持默认不排除。
                    ..Default::default()
                };
                let page = conv_repo.list_paginated(user_id, &filters).await?;
                if page.items.is_empty() {
                    break;
                }
                for row in &page.items {
                    let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_default();
                    let aw = extra.get("autowork").or_else(|| extra.get("autopilot"));
                    let Some(aw) = aw else { continue };
                    let enabled = aw.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                    let tag = aw.get("tag").and_then(|v| v.as_str()).map(|s| s.to_string());
                    if let (true, Some(tag)) = (enabled, tag) {
                        by_tag.entry(tag).or_default().push(TagBinding {
                            kind: AutoWorkTargetKind::Conversation,
                            target_id: row.id.to_string(),
                            name: if row.name.is_empty() {
                                row.id.to_string()
                            } else {
                                row.name.clone()
                            },
                            run_state: AutoWorkRunState::Idle,
                        });
                    }
                }
                if !page.has_more {
                    break;
                }
                cursor = page.items.last().map(|r| r.id);
            }
        }

        // Terminals: enumerate the user's sessions and parse the `autowork` column.
        if let Some(term_repo) = &self.terminal_repo {
            for row in term_repo.list_by_user(user_id).await? {
                let Some(blob) = row.autowork.as_deref() else { continue };
                let aw: serde_json::Value = serde_json::from_str(blob).unwrap_or_default();
                let enabled = aw.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                let tag = aw.get("tag").and_then(|v| v.as_str()).map(|s| s.to_string());
                if let (true, Some(tag)) = (enabled, tag) {
                    by_tag.entry(tag).or_default().push(TagBinding {
                        kind: AutoWorkTargetKind::Terminal,
                        target_id: row.id.to_string(),
                        name: if row.name.is_empty() {
                            row.id.to_string()
                        } else {
                            row.name.clone()
                        },
                        run_state: AutoWorkRunState::Idle,
                    });
                }
            }
        }

        Ok(by_tag
            .into_iter()
            .map(|(tag, bindings)| TagBindings { tag, bindings })
            .collect())
    }
}

/// Conversation-delete hook (spec §9.B): when an owning `conv_*` conversation is
/// deleted, clear the dual-domain owner of every requirement it owned. There is
/// no FK to cascade (a single `owner_session_id` column addresses two tables),
/// so the deletion path drives this explicitly. Wired in `nomifun-app` via
/// `ConversationService::with_delete_hook`.
#[async_trait::async_trait]
impl nomifun_common::OnConversationDelete for RequirementService {
    async fn on_conversation_deleted(&self, conversation_id: i64) {
        if let Err(e) = self
            .clear_owner_for_session(conversation_id, AutoWorkTargetKind::Conversation)
            .await
        {
            warn!(
                conversation_id,
                error = %nomifun_common::ErrorChain(&e),
                "failed to clear requirement owner on conversation delete"
            );
        }
    }
}

/// Terminal-delete hook (spec §9.B): mirror of `OnConversationDelete` for the
/// `term_*` owner domain. Wired in `nomifun-app` via
/// `TerminalService::with_delete_hook`.
#[async_trait::async_trait]
impl nomifun_common::OnTerminalDelete for RequirementService {
    async fn on_terminal_deleted(&self, terminal_id: i64) {
        if let Err(e) = self
            .clear_owner_for_session(terminal_id, AutoWorkTargetKind::Terminal)
            .await
        {
            warn!(
                terminal_id,
                error = %nomifun_common::ErrorChain(&e),
                "failed to clear requirement owner on terminal delete"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_db::SqliteRequirementRepository;
    use nomifun_db::init_database_memory;
    use nomifun_realtime::EventBroadcaster;

    #[derive(Default)]
    struct NoopBroadcaster;
    impl EventBroadcaster for NoopBroadcaster {
        fn broadcast(&self, _event: nomifun_api_types::WebSocketMessage<serde_json::Value>) {}
    }

    async fn svc() -> RequirementService {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IRequirementRepository> = Arc::new(SqliteRequirementRepository::new(db.pool().clone()));
        let emitter = RequirementEventEmitter::new(Arc::new(NoopBroadcaster));
        // Seed a user + conversation so the conversation_id FK that `claim_next`
        // sets is satisfiable.
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
             VALUES ('user_1', 'tester', 'hash', 0, 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) \
             VALUES (1, 'user_1', 'Test Conv', 'nomi', 0, 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        // Keep the DB alive for the duration of the service via a leak in tests.
        Box::leak(Box::new(db));
        RequirementService::new(repo, emitter)
    }

    async fn svc_with_attachments() -> (RequirementService, tempfile::TempDir, tempfile::TempDir) {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IRequirementRepository> = Arc::new(SqliteRequirementRepository::new(db.pool().clone()));
        let att_repo: Arc<dyn nomifun_db::IAttachmentRepository> =
            Arc::new(nomifun_db::SqliteAttachmentRepository::new(db.pool().clone()));
        let emitter = RequirementEventEmitter::new(Arc::new(NoopBroadcaster));
        Box::leak(Box::new(db));
        let data_dir = tempfile::tempdir().unwrap();
        let upload_root = tempfile::tempdir().unwrap();
        let store = crate::attachments::AttachmentStore::new(data_dir.path().to_path_buf(), att_repo)
            .with_upload_root(upload_root.path().to_path_buf());
        let svc = RequirementService::new(repo, emitter).with_attachment_store(Arc::new(store));
        (svc, data_dir, upload_root)
    }

    fn upload_png(root: &std::path::Path, name: &str) -> String {
        let p = root.join(name);
        std::fs::write(&p, b"png").unwrap();
        p.to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn create_with_attachments_binds_and_returns_dtos() {
        let (s, data_dir, upload_root) = svc_with_attachments().await;
        let src = upload_png(upload_root.path(), "a.png");
        let created = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "t".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![nomifun_api_types::NewAttachmentRef {
                    source_path: src,
                    file_name: "a.png".into(),
                }],
            })
            .await
            .unwrap();
        assert_eq!(created.attachments.len(), 1);
        assert_eq!(created.attachments[0].file_name, "a.png");
        assert!(std::path::Path::new(&created.attachments[0].abs_path).exists());
        assert!(created.attachments[0].abs_path.starts_with(data_dir.path().to_string_lossy().as_ref()));
        // get() returns them too
        let got = s.get(created.id).await.unwrap();
        assert_eq!(got.attachments.len(), 1);
    }

    #[tokio::test]
    async fn create_with_bad_attachment_rolls_back_requirement() {
        let (s, _data, upload_root) = svc_with_attachments().await;
        let src = upload_png(upload_root.path(), "a.txt"); // wrong extension
        let err = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "t".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![nomifun_api_types::NewAttachmentRef {
                    source_path: src,
                    file_name: "a.txt".into(),
                }],
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
        // the requirement row must NOT survive
        let page = s.list(&ListRequirementsQuery::default()).await.unwrap();
        assert_eq!(page.total, 0, "failed attachment bind must roll the create back");
    }

    #[tokio::test]
    async fn update_adds_and_removes_attachments() {
        let (s, data_dir, upload_root) = svc_with_attachments().await;
        let a = upload_png(upload_root.path(), "a.png");
        let created = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "t".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![nomifun_api_types::NewAttachmentRef { source_path: a, file_name: "a.png".into() }],
            })
            .await
            .unwrap();
        let b = upload_png(upload_root.path(), "b.png");
        let updated = s
            .update(
                created.id,
                UpdateRequirementRequest {
                    title: None,
                    content: None,
                    tag: None,
                    order_key: None,
                    status: None,
                    completion_note: None,
                    add_attachments: vec![nomifun_api_types::NewAttachmentRef { source_path: b, file_name: "b.png".into() }],
                    remove_attachment_ids: vec![created.attachments[0].id.clone()],
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.attachments.len(), 1);
        assert_eq!(updated.attachments[0].file_name, "b.png");
        assert!(!std::path::Path::new(&created.attachments[0].abs_path).exists(), "removed file is deleted");
        let _ = data_dir;
    }

    #[tokio::test]
    async fn update_failed_ingest_preserves_removed_targets() {
        // update() carrying BOTH removals and additions must be safe when the
        // ingest fails: the requirement's existing attachments (rows AND files)
        // must survive untouched — i.e. removal must not have been applied.
        let (s, _data, upload_root) = svc_with_attachments().await;
        let a = upload_png(upload_root.path(), "a.png");
        let created = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "t".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![nomifun_api_types::NewAttachmentRef { source_path: a, file_name: "a.png".into() }],
            })
            .await
            .unwrap();
        let bad = upload_png(upload_root.path(), "bad.txt"); // illegal extension → ingest fails
        let err = s
            .update(
                created.id,
                UpdateRequirementRequest {
                    title: None,
                    content: None,
                    tag: None,
                    order_key: None,
                    status: None,
                    completion_note: None,
                    add_attachments: vec![nomifun_api_types::NewAttachmentRef { source_path: bad, file_name: "bad.txt".into() }],
                    remove_attachment_ids: vec![created.attachments[0].id.clone()],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
        // Existing attachment row and file must be intact.
        let got = s.get(created.id).await.unwrap();
        assert_eq!(got.attachments.len(), 1, "failed ingest must not apply the removal");
        assert!(
            std::path::Path::new(&created.attachments[0].abs_path).exists(),
            "the removed-target file must still exist after a failed ingest"
        );
    }

    #[tokio::test]
    async fn attachment_only_update_bumps_updated_at() {
        // An update that ONLY touches attachments must still stamp updated_at —
        // repo.update early-returns on an empty SET list, which would leave the
        // row's timestamp stale while `requirement.updated` is still emitted.
        let (s, _data, upload_root) = svc_with_attachments().await;
        let a = upload_png(upload_root.path(), "a.png");
        let created = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "t".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![nomifun_api_types::NewAttachmentRef { source_path: a, file_name: "a.png".into() }],
            })
            .await
            .unwrap();
        // now_ms is millisecond-precision — ensure a visible delta.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let b = upload_png(upload_root.path(), "b.png");
        let updated = s
            .update(
                created.id,
                UpdateRequirementRequest {
                    title: None,
                    content: None,
                    tag: None,
                    order_key: None,
                    status: None,
                    completion_note: None,
                    add_attachments: vec![nomifun_api_types::NewAttachmentRef { source_path: b, file_name: "b.png".into() }],
                    remove_attachment_ids: vec![],
                },
            )
            .await
            .unwrap();
        assert!(
            updated.updated_at > created.updated_at,
            "attachment-only update must bump updated_at ({} !> {})",
            updated.updated_at,
            created.updated_at
        );
    }

    #[tokio::test]
    async fn delete_cleans_attachment_rows_and_files() {
        let (s, data_dir, upload_root) = svc_with_attachments().await;
        let a = upload_png(upload_root.path(), "a.png");
        let created = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "t".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![nomifun_api_types::NewAttachmentRef { source_path: a, file_name: "a.png".into() }],
            })
            .await
            .unwrap();
        s.delete(created.id).await.unwrap();
        assert!(!data_dir.path().join("attachments").join(created.id.to_string()).exists());
    }

    #[tokio::test]
    async fn create_requires_title_and_tag() {
        let s = svc().await;
        let err = s
            .create(CreateRequirementRequest {
                title: "  ".into(),
                content: String::new(),
                tag: "t".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn create_then_get_and_list() {
        let s = svc().await;
        let created = s
            .create(CreateRequirementRequest {
                title: "First".into(),
                content: "body".into(),
                tag: "alpha".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        assert_eq!(created.status, RequirementStatus::Pending);
        assert_eq!(created.order_key, "1");

        let fetched = s.get(created.id).await.unwrap();
        assert_eq!(fetched.id, created.id);

        let page = s
            .list(&ListRequirementsQuery {
                tag: Some("alpha".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items.len(), 1);
    }

    #[tokio::test]
    async fn get_missing_is_not_found() {
        let s = svc().await;
        let err = s.get(999_999).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_recomputes_sort_seq_and_board_groups() {
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "g".into(),
                order_key: Some("2".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();

        s.update(
            r.id,
            UpdateRequirementRequest {
                status: Some(RequirementStatus::Done),
                order_key: Some("1.1".into()),
                title: None,
                content: None,
                tag: None,
                completion_note: Some("did it".into()),
                add_attachments: vec![],
                remove_attachment_ids: vec![],
            },
        )
        .await
        .unwrap();

        let board = s.board("g").await.unwrap();
        assert_eq!(board.done.len(), 1);
        assert_eq!(board.done[0].order_key, "1.1");
        assert_eq!(board.pending.len(), 0);

        let tags = s.tags().await.unwrap();
        let g = tags.iter().find(|t| t.tag == "g").unwrap();
        assert_eq!(g.done, 1);
        assert_eq!(g.total, 1);
    }

    #[tokio::test]
    async fn claim_set_status_and_finalize_respects_terminal() {
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "auto".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();

        let claimed = s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
        assert_eq!(claimed.id, r.id);
        assert_eq!(claimed.status, RequirementStatus::InProgress);

        // Agent self-reports done → finalize must respect it (note preserved).
        s.complete(r.id, Some("agent did it".into())).await.unwrap();
        let finalized = s.finalize_if_needed(r.id, false, None, false).await.unwrap().unwrap();
        assert_eq!(finalized.status, RequirementStatus::Done);
        assert_eq!(finalized.completion_note.as_deref(), Some("agent did it"));
    }

    #[tokio::test]
    async fn finalize_success_marks_done_when_agent_silent() {
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "auto".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
        let done = s.finalize_if_needed(r.id, false, None, false).await.unwrap().unwrap();
        assert_eq!(done.status, RequirementStatus::Done);
    }

    #[tokio::test]
    async fn verdict_with_no_note_clears_a_stale_completion_note() {
        // A prior attempt can leave prose in completion_note (e.g. a conversation
        // turn whose agent couldn't call requirement_complete parked the requirement
        // as needs_review with its apology as the note). When the requirement later
        // reaches a verdict with NO note — exactly what the terminal path passes
        // (`finalize_if_needed(.., None, false)`) — the stale note must be cleared,
        // not preserved, or a `done` requirement shows a misleading completion record.
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "auto".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
        // Prior attempt parked it as needs_review with an apology note.
        s.set_status(r.id, RequirementStatus::NeedsReview, Some("你好\n\n无法调用 requirement_complete".into()))
            .await
            .unwrap();
        // Terminal completion: done with no note → must clear the stale note.
        let done = s.set_status(r.id, RequirementStatus::Done, None).await.unwrap();
        assert_eq!(done.status, RequirementStatus::Done);
        assert_eq!(done.completion_note, None, "a no-note verdict must clear the stale completion note");
    }

    #[tokio::test]
    async fn finalize_success_records_agent_note_when_supplied() {
        // Tool-free engines (ACP/codex/gemini) have no native completion tool, so
        // the orchestrator passes the agent's final plain-text message as the note.
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "auto".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
        let done = s
            .finalize_if_needed(r.id, false, Some("  added the logout button  ".into()), false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.status, RequirementStatus::Done);
        // Note is trimmed and recorded as the completion note.
        assert_eq!(done.completion_note.as_deref(), Some("added the logout button"));
    }

    #[tokio::test]
    async fn finalize_clean_turn_needs_review_when_verdict_expected() {
        // Engine HAD a declaration channel (expects_verdict=true) but the agent
        // ended the turn without declaring done/failed → park for human review,
        // NOT silently done. This is the soft-failure guard.
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "auto".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
        let out = s
            .finalize_if_needed(r.id, false, Some("I think I'm done".into()), true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.status, RequirementStatus::NeedsReview, "no explicit verdict → needs_review");
        assert_eq!(out.completion_note.as_deref(), Some("I think I'm done"));
        // needs_review does NOT pause the tag (it's "verify", not "failed").
        assert!(!s.is_tag_paused("auto").await.unwrap());
        // …and it is surfaced in board + tag counts.
        let board = s.board("auto").await.unwrap();
        assert_eq!(board.needs_review.len(), 1);
        let tags = s.tags().await.unwrap();
        assert_eq!(tags.iter().find(|t| t.tag == "auto").unwrap().needs_review, 1);
    }

    #[tokio::test]
    async fn finalize_respects_agent_declared_done_even_when_verdict_expected() {
        // The agent explicitly completed via its tool (row already `done`) →
        // finalize respects it even though expects_verdict=true.
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "auto".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
        s.complete(r.id, Some("agent did it".into())).await.unwrap();
        let out = s.finalize_if_needed(r.id, false, None, true).await.unwrap().unwrap();
        assert_eq!(out.status, RequirementStatus::Done);
        assert_eq!(out.completion_note.as_deref(), Some("agent did it"));
    }

    #[tokio::test]
    async fn needs_review_is_not_frozen_and_db_roundtrips() {
        // A human can move a needs_review requirement to done/failed (not frozen).
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "auto".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
        s.finalize_if_needed(r.id, false, None, true).await.unwrap();
        assert_eq!(s.get(r.id).await.unwrap().status, RequirementStatus::NeedsReview);
        // human verifies → done
        let done = s.set_status(r.id, RequirementStatus::Done, None).await.unwrap();
        assert_eq!(done.status, RequirementStatus::Done);
    }

    #[tokio::test]
    async fn finalize_error_repends_until_exhausted() {
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "auto".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();

        // Attempt 1: claim (attempt_count=1) → error → re-pend.
        s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
        let after1 = s.finalize_if_needed(r.id, true, None, false).await.unwrap().unwrap();
        assert_eq!(after1.status, RequirementStatus::Pending);

        // Attempt 2 (count=2) → error → re-pend.
        s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
        let after2 = s.finalize_if_needed(r.id, true, None, false).await.unwrap().unwrap();
        assert_eq!(after2.status, RequirementStatus::Pending);

        // Attempt 3 (count=3) → error → exhausted → failed.
        s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
        let after3 = s.finalize_if_needed(r.id, true, None, false).await.unwrap().unwrap();
        assert_eq!(after3.status, RequirementStatus::Failed);
    }

    #[tokio::test]
    async fn finalize_exhausted_pauses_tag() {
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "auto".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        assert!(!s.is_tag_paused("auto").await.unwrap());

        // Burn all 3 attempts → failed → tag must pause.
        for _ in 0..3 {
            s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
            s.finalize_if_needed(r.id, true, None, false).await.unwrap();
        }
        assert_eq!(s.get(r.id).await.unwrap().status, RequirementStatus::Failed);
        assert!(s.is_tag_paused("auto").await.unwrap(), "exhausted failure must pause the tag");

        let st = s.tag_state("auto").await.unwrap().expect("tag state row");
        assert_eq!(st.paused_reason.as_deref(), Some("requirement_failed"));
        assert_eq!(st.paused_req_id, Some(r.id));
    }

    #[tokio::test]
    async fn resume_tag_unpauses_and_requeues_failed() {
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "auto".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        for _ in 0..3 {
            s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
            s.finalize_if_needed(r.id, true, None, false).await.unwrap();
        }
        assert!(s.is_tag_paused("auto").await.unwrap());

        // Resume + requeue the failed requirement.
        s.resume_tag("auto", &[r.id.clone()]).await.unwrap();
        assert!(!s.is_tag_paused("auto").await.unwrap());
        let row = s.get(r.id).await.unwrap();
        assert_eq!(row.status, RequirementStatus::Pending, "requeued back to pending");
        assert_eq!(row.attempt_count, 0, "requeue clears consumed attempts");
        // And it is claimable again now the tag is resumed.
        let claimed = s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap();
        assert_eq!(claimed.expect("claimable after resume").id, r.id);
    }

    #[tokio::test]
    async fn resume_tag_for_enable_unpauses_and_refreshes_stuck_requirement() {
        // The recurring "彻底不工作" trap: a tag paused by a prior failure blocks
        // every conversation bound to it. Re-enabling AutoWork must auto-resume it
        // AND give the stuck requirement a fresh attempt budget — without the
        // caller passing explicit ids.
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "auto".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        for _ in 0..3 {
            s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
            s.finalize_if_needed(r.id, true, None, false).await.unwrap();
        }
        assert!(s.is_tag_paused("auto").await.unwrap(), "3 failures pause the tag");

        // Enabling AutoWork on a conversation bound to this tag auto-resumes it.
        s.resume_tag_for_enable("auto").await.unwrap();
        assert!(!s.is_tag_paused("auto").await.unwrap(), "enable must unpause the tag");
        let row = s.get(r.id).await.unwrap();
        assert_eq!(row.status, RequirementStatus::Pending, "stuck requirement re-pended");
        assert_eq!(row.attempt_count, 0, "fresh attempt budget on enable");
        // Claimable again.
        let claimed = s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap();
        assert_eq!(claimed.expect("claimable after enable-resume").id, r.id);
    }

    #[tokio::test]
    async fn resume_tag_for_enable_is_noop_when_not_paused() {
        // Must NOT disturb a healthy tag: no pause → no requeue, no attempt reset.
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "auto".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        // Consume one attempt but do NOT exhaust (tag stays active).
        s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
        s.finalize_if_needed(r.id, true, None, false).await.unwrap(); // re-pended, attempt_count=1
        assert!(!s.is_tag_paused("auto").await.unwrap());

        s.resume_tag_for_enable("auto").await.unwrap();
        let row = s.get(r.id).await.unwrap();
        assert_eq!(row.attempt_count, 1, "an unpaused tag must not be reset by enable");
    }

    #[tokio::test]
    async fn tags_reports_paused_state() {
        let s = svc().await;
        s.create(CreateRequirementRequest {
            title: "T".into(),
            content: String::new(),
            tag: "auto".into(),
            order_key: Some("1".into()),
            status: None,
            created_by: None,
            attachments: vec![],
        })
        .await
        .unwrap();

        let tags = s.tags().await.unwrap();
        assert!(!tags.iter().find(|t| t.tag == "auto").unwrap().paused);

        s.repo()
            .pause_tag("auto", "requirement_failed", None, now_ms())
            .await
            .unwrap();
        let tags2 = s.tags().await.unwrap();
        let t = tags2.iter().find(|t| t.tag == "auto").unwrap();
        assert!(t.paused, "tags() must report the paused tag");
        assert_eq!(t.paused_reason.as_deref(), Some("requirement_failed"));
    }

    #[tokio::test]
    async fn unclaim_busy_reverts_without_consuming_attempt() {
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "auto".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        let claimed = s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
        assert_eq!(claimed.attempt_count, 1);
        s.unclaim_busy(r.id, 1).await.unwrap();
        let after = s.get(r.id).await.unwrap();
        assert_eq!(after.status, RequirementStatus::Pending);
        assert_eq!(after.attempt_count, 0, "busy unclaim must not consume an attempt");
    }

    #[tokio::test]
    async fn clear_owner_for_session_clears_owner_columns_and_repends_in_progress() {
        // §9.B: deleting a conversation/terminal must clear the dual-domain
        // `owner_session_id`+`owner_kind` (no FK to cascade) of every requirement
        // it owned, and re-pend any in_progress one (its session is gone).
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "auto".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        let claimed = s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
        // Claimed → owner columns are set and paired.
        assert_eq!(claimed.owner_session_id, Some(1));
        assert_eq!(claimed.owner_kind.as_deref(), Some("conversation"));
        assert_eq!(claimed.status, RequirementStatus::InProgress);

        // The owning conversation is deleted → clear its requirements' owner.
        let cleared = s.clear_owner_for_session(1, AutoWorkTargetKind::Conversation).await.unwrap();
        assert_eq!(cleared, 1, "the one claimed requirement must be cleared");
        let after = s.get(r.id).await.unwrap();
        // Both owner columns cleared together (paired-NULL CHECK), in_progress
        // re-pended, attempt NOT consumed (session vanishing is not a failed try).
        assert_eq!(after.owner_session_id, None);
        assert_eq!(after.owner_kind, None);
        assert_eq!(after.status, RequirementStatus::Pending);
        assert_eq!(after.attempt_count, 1, "clearing owner must not consume an attempt");

        // A session that owns nothing → no-op, zero cleared, idempotent.
        assert_eq!(s.clear_owner_for_session(1, AutoWorkTargetKind::Conversation).await.unwrap(), 0);
        assert_eq!(s.clear_owner_for_session(999, AutoWorkTargetKind::Terminal).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn set_status_rejects_transition_out_of_terminal() {
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "t".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        s.set_status(r.id, RequirementStatus::Done, None).await.unwrap();
        // done is terminal → cannot go back to in_progress.
        let err = s
            .set_status(r.id, RequirementStatus::InProgress, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
        // re-setting the same terminal status is an idempotent no-op.
        let again = s.set_status(r.id, RequirementStatus::Done, None).await.unwrap();
        assert_eq!(again.status, RequirementStatus::Done);
    }

    #[tokio::test]
    async fn release_claim_repends_without_consuming_attempt() {
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "auto".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        let claimed = s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
        assert_eq!(claimed.attempt_count, 1);
        s.release_claim(r.id, 1).await.unwrap();
        let after = s.get(r.id).await.unwrap();
        assert_eq!(after.status, RequirementStatus::Pending);
        assert_eq!(after.attempt_count, 1, "release must not consume an attempt");
        // wrong-owner release is a no-op.
        s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
        s.release_claim(r.id, 2).await.unwrap();
        assert_eq!(s.get(r.id).await.unwrap().status, RequirementStatus::InProgress);
    }

    #[tokio::test]
    async fn user_interrupt_pauses_tag_and_repends_without_consuming_attempt() {
        let s = svc().await;
        let r = s
            .create(CreateRequirementRequest {
                title: "T".into(),
                content: String::new(),
                tag: "auto".into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        let claimed = s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().unwrap();
        assert_eq!(claimed.attempt_count, 1);

        s.user_interrupt(r.id, 1, "auto").await.unwrap();

        // The requirement goes back to pending WITHOUT a consumed attempt — a
        // user stop is not a failed attempt.
        let after = s.get(r.id).await.unwrap();
        assert_eq!(after.status, RequirementStatus::Pending);
        assert_eq!(after.attempt_count, 1, "user interrupt must not consume an attempt");

        // The tag is paused (reason user_interrupted) so the loop cannot
        // instantly re-claim and re-inject — the "paused it and it started
        // running again by itself" bug.
        assert!(s.is_tag_paused("auto").await.unwrap(), "user interrupt must pause the tag");
        let st = s.tag_state("auto").await.unwrap().expect("tag state row");
        assert_eq!(st.paused_reason.as_deref(), Some("user_interrupted"));
        assert_eq!(st.paused_req_id, Some(r.id));
        assert!(
            s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().is_none(),
            "a paused tag must not be claimable"
        );

        // Resume (the UI's 恢复 button) makes it claimable again.
        s.resume_tag("auto", &[]).await.unwrap();
        assert!(s.claim_next("auto", 1, AutoWorkTargetKind::Conversation, 60_000).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn delete_many_counts_and_skips_missing() {
        let s = svc().await;
        let mut ids = Vec::new();
        for i in 0..2 {
            let r = s
                .create(CreateRequirementRequest {
                    title: format!("T{i}"),
                    content: String::new(),
                    tag: "t".into(),
                    order_key: Some(format!("{i}")),
                    status: None,
                    created_by: None,
                    attachments: vec![],
                })
                .await
                .unwrap();
            ids.push(r.id);
        }
        ids.push(999_999);
        let deleted = s.delete_many(&ids).await.unwrap();
        assert_eq!(deleted, 2, "missing ids are skipped");
    }

    // --- Terminal AutoWork (mock driver) --------------------------------

    use nomifun_terminal::TerminalDescription;
    use nomifun_terminal::error::TerminalError;
    use std::sync::Mutex as StdMutex;

    struct MockDriver {
        user_id: String,
        command: String,
        args: Vec<String>,
        backend: Option<String>,
        last_status: String,
        exists: bool,
        autowork: StdMutex<Option<String>>,
        idmm: StdMutex<Option<String>>,
    }

    impl MockDriver {
        fn agent() -> Self {
            Self {
                user_id: "user_1".into(),
                // Empty command/no-args: eligibility for the existing cases is
                // driven purely by the declared `backend` (which `resolve_agent_family`
                // checks first). The wrapper/custom-command tests set command/args
                // explicitly to exercise the program-stem / arg-token paths.
                command: String::new(),
                args: vec![],
                backend: Some("claude".into()),
                last_status: "running".into(),
                exists: true,
                autowork: StdMutex::new(None),
                idmm: StdMutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl TerminalDriver for MockDriver {
        async fn write_input(&self, _id: i64, _bytes: &[u8]) -> Result<(), TerminalError> {
            Ok(())
        }
        fn subscribe_output(&self, _id: i64) -> Option<tokio::sync::broadcast::Receiver<Vec<u8>>> {
            None
        }
        fn is_alive(&self, _id: i64) -> bool {
            self.last_status == "running"
        }
        async fn describe(&self, _id: i64) -> Result<Option<TerminalDescription>, TerminalError> {
            if !self.exists {
                return Ok(None);
            }
            Ok(Some(TerminalDescription {
                user_id: self.user_id.clone(),
                cwd: String::new(),
                command: self.command.clone(),
                args: self.args.clone(),
                backend: self.backend.clone(),
                mode: None,
                last_status: self.last_status.clone(),
            }))
        }
        async fn read_autowork(&self, _id: i64) -> Result<Option<String>, TerminalError> {
            Ok(self.autowork.lock().unwrap().clone())
        }
        async fn write_autowork(&self, _id: i64, autowork: Option<&str>) -> Result<(), TerminalError> {
            *self.autowork.lock().unwrap() = autowork.map(|s| s.to_owned());
            Ok(())
        }
        async fn read_idmm(&self, _id: i64) -> Result<Option<String>, TerminalError> {
            Ok(self.idmm.lock().unwrap().clone())
        }
        async fn write_idmm(&self, _id: i64, idmm: Option<&str>) -> Result<(), TerminalError> {
            *self.idmm.lock().unwrap() = idmm.map(|s| s.to_owned());
            Ok(())
        }
        fn subscribe_lifecycle(
            &self,
            _id: i64,
        ) -> Option<tokio::sync::broadcast::Receiver<nomifun_terminal::TerminalLifecycleEvent>> {
            None
        }
    }

    async fn svc_with_driver(driver: Arc<dyn TerminalDriver>) -> RequirementService {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IRequirementRepository> = Arc::new(SqliteRequirementRepository::new(db.pool().clone()));
        let emitter = RequirementEventEmitter::new(Arc::new(NoopBroadcaster));
        Box::leak(Box::new(db));
        RequirementService::new(repo, emitter).with_terminal_driver(driver)
    }

    #[tokio::test]
    async fn terminal_config_save_and_read_roundtrips() {
        let driver = Arc::new(MockDriver::agent());
        let s = svc_with_driver(driver).await;
        s.save_autowork_config(AutoWorkTargetKind::Terminal, "1", true, Some("alpha"), Some(5))
            .await
            .unwrap();
        let (enabled, tag, max) = s
            .read_autowork_config(AutoWorkTargetKind::Terminal, "1")
            .await
            .unwrap();
        assert!(enabled);
        assert_eq!(tag.as_deref(), Some("alpha"));
        assert_eq!(max, Some(5));
    }

    #[tokio::test]
    async fn verify_terminal_owner_enforces_isolation() {
        let s = svc_with_driver(Arc::new(MockDriver::agent())).await;
        // owner matches
        s.verify_terminal_owner("1", "user_1").await.unwrap();
        // wrong owner → Forbidden
        assert!(matches!(
            s.verify_terminal_owner("1", "intruder").await.unwrap_err(),
            AppError::Forbidden(_)
        ));
        // missing terminal → NotFound
        let missing = Arc::new(MockDriver {
            exists: false,
            ..MockDriver::agent()
        });
        let s2 = svc_with_driver(missing).await;
        assert!(matches!(
            s2.verify_terminal_owner("999", "user_1").await.unwrap_err(),
            AppError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn ensure_terminal_autowork_eligible_gates_backend_and_status() {
        // agent CLI + running → ok
        let s = svc_with_driver(Arc::new(MockDriver::agent())).await;
        s.ensure_terminal_autowork_eligible("1").await.unwrap();

        // plain shell (backend None) → BadRequest
        let shell = Arc::new(MockDriver {
            backend: None,
            ..MockDriver::agent()
        });
        let s_shell = svc_with_driver(shell).await;
        assert!(matches!(
            s_shell.ensure_terminal_autowork_eligible("1").await.unwrap_err(),
            AppError::BadRequest(_)
        ));

        // agent CLI but exited → BadRequest
        let exited = Arc::new(MockDriver {
            last_status: "exited".into(),
            ..MockDriver::agent()
        });
        let s_exited = svc_with_driver(exited).await;
        assert!(matches!(
            s_exited.ensure_terminal_autowork_eligible("1").await.unwrap_err(),
            AppError::BadRequest(_)
        ));
    }

    #[tokio::test]
    async fn ensure_terminal_autowork_eligible_verdict_capable_only() {
        // codex is verdict-capable → ok
        let codex = Arc::new(MockDriver {
            backend: Some("codex".into()),
            ..MockDriver::agent()
        });
        let s_codex = svc_with_driver(codex).await;
        s_codex.ensure_terminal_autowork_eligible("1").await.unwrap();

        // gemini is NOT verdict-capable (no lifecycle hook, no MCP profile) → BadRequest
        let gemini = Arc::new(MockDriver {
            backend: Some("gemini".into()),
            ..MockDriver::agent()
        });
        let s_gemini = svc_with_driver(gemini).await;
        assert!(matches!(
            s_gemini.ensure_terminal_autowork_eligible("1").await.unwrap_err(),
            AppError::BadRequest(_)
        ));

        // unknown CLI (e.g. "aider") → BadRequest
        let unknown = Arc::new(MockDriver {
            backend: Some("aider".into()),
            ..MockDriver::agent()
        });
        let s_unknown = svc_with_driver(unknown).await;
        assert!(matches!(
            s_unknown.ensure_terminal_autowork_eligible("1").await.unwrap_err(),
            AppError::BadRequest(_)
        ));
    }

    #[tokio::test]
    async fn ensure_terminal_autowork_eligible_accepts_wrappers_and_custom_commands() {
        // The bug this fixes: a wrapper / custom launch stores `backend = None`
        // (no preset declared it), yet the launch injector resolves it to an
        // agent family via the command/args and DOES inject the lifecycle hook.
        // Eligibility must agree — it now resolves the family the same way.

        // Wrapper `stepcode claude` with no declared backend → eligible.
        let wrapper = Arc::new(MockDriver {
            command: "stepcode".into(),
            args: vec!["claude".into()],
            backend: None,
            ..MockDriver::agent()
        });
        svc_with_driver(wrapper).await.ensure_terminal_autowork_eligible("1").await.unwrap();

        // `npx codex` wrapper → eligible.
        let npx = Arc::new(MockDriver {
            command: "npx".into(),
            args: vec!["codex".into()],
            backend: None,
            ..MockDriver::agent()
        });
        svc_with_driver(npx).await.ensure_terminal_autowork_eligible("1").await.unwrap();

        // Bare `claude` typed into a custom (shell-preset) command, backend None → eligible.
        let bare = Arc::new(MockDriver {
            command: "claude".into(),
            args: vec!["--dangerously-skip-permissions".into()],
            backend: None,
            ..MockDriver::agent()
        });
        svc_with_driver(bare).await.ensure_terminal_autowork_eligible("1").await.unwrap();

        // A wrapper around a non-agent (e.g. `stepcode frob`) → still rejected.
        let bad_wrapper = Arc::new(MockDriver {
            command: "stepcode".into(),
            args: vec!["frob".into()],
            backend: None,
            ..MockDriver::agent()
        });
        assert!(matches!(
            svc_with_driver(bad_wrapper).await.ensure_terminal_autowork_eligible("1").await.unwrap_err(),
            AppError::BadRequest(_)
        ));
    }

    // ── C2 (spec §2.2): cross-domain release isolation ──────────────────────
    //
    // After integerization `conv#5` and `term#5` share the numeric owner value
    // `5`. `release_claim` is a CONVERSATION-domain operation; it must NOT
    // release a requirement owned by a TERMINAL that merely shares the number.

    /// Build a service over a fresh memory DB with BOTH a conversation #5 and a
    /// terminal #5 seeded (so the dual-domain owner column can hold either at
    /// the same numeric id). Returns the live service (DB leaked to keep it
    /// alive, mirroring `svc()`).
    async fn svc_with_conv5_and_term5() -> RequirementService {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IRequirementRepository> = Arc::new(SqliteRequirementRepository::new(db.pool().clone()));
        let emitter = RequirementEventEmitter::new(Arc::new(NoopBroadcaster));
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
             VALUES ('user_1', 'tester', 'hash', 0, 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        // Force a conversation with id == 5.
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) \
             VALUES (5, 'user_1', 'Conv Five', 'nomi', 0, 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        // Force a terminal with id == 5 (same number, different domain).
        sqlx::query(
            "INSERT INTO terminal_sessions \
                 (id, user_id, name, cwd, command, args, cols, rows, last_status, created_at, updated_at) \
             VALUES (5, 'user_1', 'Term Five', '/tmp', 'bash', '[]', 80, 24, 'running', 0, 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        Box::leak(Box::new(db));
        RequirementService::new(repo, emitter)
    }

    #[tokio::test]
    async fn c2_conversation_release_does_not_release_terminal_owned_requirement() {
        let s = svc_with_conv5_and_term5().await;
        // A requirement claimed by TERMINAL #5 (owner_kind = "terminal").
        let req = s
            .create(CreateRequirementRequest {
                title: "term work".into(),
                content: String::new(),
                tag: "t".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        let claimed = s
            .claim_next("t", 5, AutoWorkTargetKind::Terminal, 60_000)
            .await
            .unwrap()
            .expect("a pending requirement is claimable");
        assert_eq!(claimed.owner_session_id, Some(5));
        assert_eq!(claimed.owner_kind.as_deref(), Some("terminal"));

        // CONVERSATION #5 tries to release it (numeric owner matches: 5 == 5).
        // It must be a NO-OP — the requirement stays in_progress, terminal-owned.
        s.release_claim(req.id, 5).await.unwrap();

        let after = s.get(req.id).await.unwrap();
        assert_eq!(
            after.status,
            RequirementStatus::InProgress,
            "conversation #5 must NOT release a requirement owned by terminal #5"
        );
        assert_eq!(after.owner_session_id, Some(5));
        assert_eq!(after.owner_kind.as_deref(), Some("terminal"));
    }

    #[tokio::test]
    async fn c2_conversation_release_does_release_own_conversation_requirement() {
        // The positive control: a CONVERSATION #5-owned requirement IS released
        // by conversation #5 (so the fix did not over-restrict the happy path).
        let s = svc_with_conv5_and_term5().await;
        let req = s
            .create(CreateRequirementRequest {
                title: "conv work".into(),
                content: String::new(),
                tag: "t".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        let claimed = s
            .claim_next("t", 5, AutoWorkTargetKind::Conversation, 60_000)
            .await
            .unwrap()
            .expect("claimable");
        assert_eq!(claimed.owner_kind.as_deref(), Some("conversation"));

        s.release_claim(req.id, 5).await.unwrap();

        let after = s.get(req.id).await.unwrap();
        assert_eq!(after.status, RequirementStatus::Pending);
        assert_eq!(after.owner_session_id, None);
        assert_eq!(after.owner_kind, None);
    }

    #[tokio::test]
    async fn c2_clear_owner_for_session_is_domain_scoped() {
        // `clear_owner_for_session(5, Conversation)` must clear ONLY conv#5's
        // requirements, never term#5's — even though both owners are the
        // integer 5 (spec §2.2, the C2-adjacent cross-domain path).
        let s = svc_with_conv5_and_term5().await;
        // One requirement owned by conv#5, one by term#5, both in tag "t".
        let conv_req = s
            .create(CreateRequirementRequest {
                title: "conv".into(),
                content: String::new(),
                tag: "t".into(),
                order_key: Some("a".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        let term_req = s
            .create(CreateRequirementRequest {
                title: "term".into(),
                content: String::new(),
                tag: "t2".into(),
                order_key: Some("a".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        // Claim each into its own domain at id 5.
        s.claim_next("t", 5, AutoWorkTargetKind::Conversation, 60_000)
            .await
            .unwrap()
            .expect("conv claim");
        s.claim_next("t2", 5, AutoWorkTargetKind::Terminal, 60_000)
            .await
            .unwrap()
            .expect("term claim");

        // Delete-hook for conversation #5: clears the conversation domain only.
        let cleared = s
            .clear_owner_for_session(5, AutoWorkTargetKind::Conversation)
            .await
            .unwrap();
        assert_eq!(cleared, 1, "exactly the conv#5-owned requirement is cleared");

        let conv_after = s.get(conv_req.id).await.unwrap();
        assert_eq!(conv_after.owner_session_id, None, "conv#5 owner cleared");
        let term_after = s.get(term_req.id).await.unwrap();
        assert_eq!(
            term_after.owner_session_id,
            Some(5),
            "term#5 owner is UNTOUCHED by a conversation-domain clear"
        );
        assert_eq!(term_after.owner_kind.as_deref(), Some("terminal"));
    }
}

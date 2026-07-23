use std::sync::Arc;

use nomifun_api_types::{
    AttachmentDto, AutoWorkRunState, AutoWorkTargetKind, BoardResponse, CreateRequirementRequest,
    ListRequirementsQuery, Requirement, RequirementStatus, TagBinding, TagBindings, TagSummary,
    UpdateRequirementRequest,
};
use nomifun_common::{
    AppError, AttachmentId, ConversationId, PaginatedResult, RequirementId, TerminalId, UserId, now_ms,
};
use nomifun_db::models::{RequirementRowUpdate, RequirementTagRow};
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

/// Default claim lease (ms). The AutoWork runner renews well within this window.
pub const DEFAULT_LEASE_MS: i64 = 120_000;
/// Max claim attempts before a requirement is left `failed` (poison-pill guard).
pub const MAX_ATTEMPTS: i64 = 3;

/// Validate an AutoWork target handle in the conversation entity domain.
fn parse_conversation_id(target_id: &str) -> Result<&str, AppError> {
    ConversationId::try_from(target_id)
        .map(|_| target_id)
        .map_err(|_| AppError::NotFound(format!("conversation {target_id}")))
}

fn parse_terminal_id(target_id: &str) -> Result<&str, AppError> {
    TerminalId::try_from(target_id)
        .map(|_| target_id)
        .map_err(|_| AppError::NotFound(format!("terminal {target_id}")))
}

fn validate_requirement_id(id: &str) -> Result<&str, AppError> {
    RequirementId::parse(id)
        .map(|_| id)
        .map_err(|error| AppError::BadRequest(format!("invalid requirement id: {error}")))
}

fn validate_attachment_ids(ids: &[String]) -> Result<(), AppError> {
    for id in ids {
        AttachmentId::try_from(id.as_str())
            .map_err(|error| AppError::BadRequest(format!("invalid attachment id: {error}")))?;
    }
    Ok(())
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
    /// webhook can notify. Optional + non-blocking —a failing webhook never
    /// affects requirement state.
    completion_notifier: Option<Arc<dyn CompletionNotifier>>,
    /// Notified whenever a requirement becomes claimable (created or re-pended),
    /// so idle AutoWork loops wake immediately instead of waiting for their poll
    /// fallback. Attached during assembly to the same `Notify` the AutoWork runner
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

    /// Attach the AutoWork waker. Shared with the runner: transitions that
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
    async fn load_attachments(&self, requirement_id: &str) -> Vec<AttachmentDto> {
        let Some(store) = &self.attachments else { return Vec::new() };
        match store.list(requirement_id).await {
            Ok(rows) => rows.iter().map(|r| store.to_dto(r)).collect(),
            Err(e) => {
                warn!(error = %e, requirement_id, "failed to load requirement attachments");
                Vec::new()
            }
        }
    }

    /// Staging entry point for the AutoWork runner: copy the requirement's
    /// attachments into the session workspace (when given) and return prompt
    /// entries. Empty when no store is attached.
    pub async fn stage_attachments_for_prompt(
        &self,
        req_id: &str,
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

    /// Expose the repo for the AutoWork runner / sweeper (Phase C).
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
        let new_row = nomifun_db::models::NewRequirementRow {
            title: req.title,
            content: req.content,
            tag: req.tag,
            sort_seq: to_sort_seq(&order_key),
            order_key,
            status: status.as_db().to_string(),
            // `priority` is a reserved technical column. V3 ordering is
            // exclusively defined by `order_key`; callers cannot supply a
            // second ordering representation.
            priority: 0,
            completion_note: None,
            owner_conversation_id: None,
            owner_terminal_id: None,
            active_turn_started_at: None,
            lease_expires_at: None,
            started_at: None,
            completed_at: None,
            attempt_count: 0,
            created_by: req.created_by.unwrap_or_else(|| "user".to_string()),
            extra: "{}".to_string(),
            created_at: now,
            updated_at: now,
        };
        let row = self.repo.insert(&new_row).await?;
        let mut dto = row_to_dto(&row);
        if !new_attachments.is_empty() {
            let Some(store) = &self.attachments else {
                let _ = self.repo.delete(&row.requirement_id).await;
                return Err(AppError::Internal("attachment store not attached".into()));
            };
            match store
                .ingest(&row.requirement_id, &new_attachments, Some(&row.created_by))
                .await
            {
                Ok(rows) => dto.attachments = rows.iter().map(|r| store.to_dto(r)).collect(),
                Err(e) => {
                    // Keep create atomic for the caller: drop the row we just inserted.
                    if let Err(de) = self.repo.delete(&row.requirement_id).await {
                        warn!(error = %de, requirement_id = row.requirement_id, "rollback after attachment ingest failure failed");
                    }
                    return Err(e);
                }
            }
        }
        self.emitter.emit_created(&dto);
        // A freshly-created pending requirement is claimable now —wake idle loops.
        if dto.status == RequirementStatus::Pending {
            self.wake_autowork();
        }
        Ok(dto)
    }

    pub async fn get(&self, id: &str) -> Result<Requirement, AppError> {
        let id = validate_requirement_id(id)?;
        let row = self
            .repo
            .get_by_requirement_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("requirement {id}")))?;
        let mut dto = row_to_dto(&row);
        dto.attachments = self.load_attachments(id).await;
        Ok(dto)
    }

    pub async fn list(&self, query: &ListRequirementsQuery) -> Result<PaginatedResult<Requirement>, AppError> {
        if let Some(conversation_id) = query.conversation_id.as_deref() {
            parse_conversation_id(conversation_id)?;
        }
        let page = query.page.unwrap_or(1).max(1);
        let page_size = query.page_size.unwrap_or(20).clamp(1, 200);
        let params = ListRequirementsParams {
            tag: query.tag.clone(),
            status: query.status.map(|s| s.as_db().to_string()),
            owner_conversation_id: query.conversation_id.clone(),
            // The public list query has no kind filter —`conversation_id` here
            // is a UI filter that historically meant the conversation domain.
            owner_terminal_id: None,
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

    pub async fn update(&self, id: &str, req: UpdateRequirementRequest) -> Result<Requirement, AppError> {
        let id = validate_requirement_id(id)?;
        validate_attachment_ids(&req.remove_attachment_ids)?;
        // Ensure it exists for a clean 404 (update() also returns NotFound).
        let row = self
            .repo
            .get_by_requirement_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("requirement {id}")))?;

        // Attachment changes first —ingest BEFORE remove. Ingest is the only
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
        // field —repo.update stamps updated_at itself.
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
            .get_by_requirement_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("requirement {id}")))?;
        let mut dto = row_to_dto(&row);
        dto.attachments = self.load_attachments(id).await;
        self.emitter.emit_updated(&dto);
        Ok(dto)
    }

    pub async fn delete(&self, id: &str) -> Result<(), AppError> {
        let id = validate_requirement_id(id)?;
        let prepared = match &self.attachments {
            Some(store) => Some((Arc::clone(store), store.prepare_delete_all(id).await?)),
            None => None,
        };
        if let Err(error) = self.repo.delete(id).await {
            if let Some((store, prepared)) = prepared
                && let Err(restore_error) = store.restore_prepared_delete(prepared).await
            {
                warn!(
                    error = %restore_error,
                    requirement_id = id,
                    "attachment rollback failed after requirement delete transaction failed"
                );
            }
            return Err(error.into());
        }
        if let Some((store, prepared)) = prepared {
            store.finish_prepared_delete(prepared).await;
        }
        self.emitter.emit_deleted(id);
        Ok(())
    }

    /// Delete many requirements by id. Missing ids are skipped (not an error).
    /// Returns the number actually deleted; emits `requirement.deleted` per row.
    pub async fn delete_many(&self, ids: &[String]) -> Result<u64, AppError> {
        for id in ids {
            validate_requirement_id(id)?;
        }
        let mut deleted = 0u64;
        for id in ids {
            let prepared = match &self.attachments {
                Some(store) => Some((Arc::clone(store), store.prepare_delete_all(id).await?)),
                None => None,
            };
            match self.repo.delete(id).await {
                Ok(()) => {
                    if let Some((store, prepared)) = prepared {
                        store.finish_prepared_delete(prepared).await;
                    }
                    self.emitter.emit_deleted(id);
                    deleted += 1;
                }
                Err(nomifun_db::DbError::NotFound(_)) => {
                    if let Some((store, prepared)) = prepared {
                        store.restore_prepared_delete(prepared).await?;
                    }
                }
                Err(error) => {
                    if let Some((store, prepared)) = prepared
                        && let Err(restore_error) = store.restore_prepared_delete(prepared).await
                    {
                        warn!(
                            error = %restore_error,
                            requirement_id = id,
                            "attachment rollback failed after batch requirement delete failed"
                        );
                    }
                    return Err(error.into());
                }
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

    /// Atomically claim the next pending requirement for `tag`. `kind` validates
    /// `owner_id` in its entity domain and selects the corresponding disjoint
    /// owner column; the other owner column stays null. Emits
    /// `requirement.statusChanged` for the claimed row.
    pub async fn claim_next(
        &self,
        tag: &str,
        owner_id: &str,
        kind: AutoWorkTargetKind,
        lease_ms: i64,
    ) -> Result<Option<Requirement>, AppError> {
        let (owner_conversation_id, owner_terminal_id) = match kind {
            AutoWorkTargetKind::Conversation => (Some(parse_conversation_id(owner_id)?), None),
            AutoWorkTargetKind::Terminal => (None, Some(parse_terminal_id(owner_id)?)),
        };
        let claimed = self
            .repo
            .claim_next(
                tag,
                owner_conversation_id,
                owner_terminal_id,
                lease_ms,
                now_ms(),
            )
            .await?;
        Ok(claimed.map(|row| {
            let dto = row_to_dto(&row);
            self.emitter.emit_status_changed(&dto);
            dto
        }))
    }

    /// Renew the lease for `id` held by `owner_id` in the requested owner domain.
    /// Returns whether a row matched.
    pub async fn renew_lease(
        &self,
        id: &str,
        owner_id: &str,
        kind: AutoWorkTargetKind,
        lease_ms: i64,
    ) -> Result<bool, AppError> {
        let id = validate_requirement_id(id)?;
        let owners = match kind {
            AutoWorkTargetKind::Conversation => (Some(parse_conversation_id(owner_id)?), None),
            AutoWorkTargetKind::Terminal => (None, Some(parse_terminal_id(owner_id)?)),
        };
        Ok(self
            .repo
            .renew_lease(id, owners.0, owners.1, lease_ms, now_ms())
            .await?)
    }

    /// Verify `conversation_id` belongs to `user_id` (data isolation for the
    /// claim / autowork routes). No-op when no conversation repo is attached
    /// (e.g. the sink-only service instance). Returns `NotFound` if the
    /// conversation does not exist, `Forbidden`
    /// if owned by another user.
    pub async fn verify_conversation_owner(&self, conversation_id: &str, user_id: &str) -> Result<(), AppError> {
        let conversation_id = parse_conversation_id(conversation_id)?;
        let user_id = UserId::parse(user_id)
            .map_err(|error| AppError::Forbidden(format!("invalid caller identity: {error}")))?;
        let Some(conv_repo) = &self.conversation_repo else {
            return Ok(());
        };
        let row = conv_repo
            .get(conversation_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("conversation {conversation_id}")))?;
        let row_user_id = UserId::parse(&row.user_id).map_err(|error| {
            AppError::Forbidden(format!("conversation {conversation_id} has invalid owner: {error}"))
        })?;
        if row_user_id != user_id {
            return Err(AppError::Forbidden(format!(
                "conversation {conversation_id} is not owned by the caller"
            )));
        }
        Ok(())
    }

    /// stopped mid-turn). No-op unless the requirement is `in_progress` and held
    /// by `conversation_id` IN THE CONVERSATION DOMAIN. Does NOT consume
    /// `attempt_count` —a user stop is not a failed attempt. Emits
    /// `requirement.statusChanged`.
    ///
    /// SECURITY (C2, spec §2.2): ownership uses disjoint conversation and
    /// terminal columns. A conversation caller can therefore never release a
    /// terminal-owned requirement, even if a malformed caller reuses its text.
    pub async fn release_claim(&self, id: &str, conversation_id: &str) -> Result<(), AppError> {
        let id = validate_requirement_id(id)?;
        let conversation_id = parse_conversation_id(conversation_id)?;
        let Some(row) = self.repo.get_by_requirement_id(id).await? else {
            return Ok(());
        };
        if row.status != "in_progress"
            || row.owner_conversation_id.as_deref() != Some(conversation_id)
        {
            return Ok(());
        }
        let params = RequirementRowUpdate {
            status: Some("pending".to_string()),
            owner_conversation_id: Some(None),
            owner_terminal_id: Some(None),
            active_turn_started_at: Some(None),
            lease_expires_at: Some(None),
            ..Default::default()
        };
        self.repo.update(id, &params).await?;
        if let Some(updated) = self.repo.get_by_requirement_id(id).await? {
            self.emitter.emit_status_changed(&row_to_dto(&updated));
        }
        // Released back to pending —another bound session may claim it now.
        self.wake_autowork();
        Ok(())
    }

    /// The user manually cancelled an AutoWork-driven turn —treat it as an
    /// explicit "stop working on this" signal, NOT a failed attempt:
    /// 1. pause the tag (reason `user_interrupted`, resumable from the UI) so
    ///    the persistent loop does not immediately re-claim and re-inject the
    ///    same requirement —the historical "I paused it and seconds later it
    ///    was running again";
    /// 2. release the claim back to `pending` WITHOUT consuming an attempt.
    /// Ordered pause-first so the release's wake cannot race a re-claim (the
    /// claim SQL skips paused tags). Best-effort on the pause write: a failure
    /// must not block the claim release.
    pub async fn user_interrupt(&self, id: &str, conversation_id: &str, tag: &str) -> Result<(), AppError> {
        let id = validate_requirement_id(id)?;
        match self.repo.pause_tag(tag, "user_interrupted", Some(id), now_ms()).await {
            Ok(()) => self.emitter.emit_tag_paused(&nomifun_api_types::TagPausedPayload {
                tag: tag.to_string(),
                reason: "user_interrupted".to_string(),
                requirement_id: Some(id.to_owned()),
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
        id: &str,
        status: RequirementStatus,
        note: Option<String>,
    ) -> Result<Requirement, AppError> {
        let id = validate_requirement_id(id)?;
        let row = self
            .repo
            .get_by_requirement_id(id)
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
            .get_by_requirement_id(id)
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
    pub async fn complete(&self, id: &str, completion_note: Option<String>) -> Result<Requirement, AppError> {
        let id = validate_requirement_id(id)?;
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
                driver.write_autowork(parse_terminal_id(target_id)?, Some(&blob)).await?;
                Ok(())
            }
        }
    }

    /// Read the persisted AutoWork config `(enabled, tag, max)` for a target.
    /// Returns `(false, None, None)` when no backing store is attached or no
    /// config exists.
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
                let Some(row) = conv_repo.get(parse_conversation_id(target_id)?).await? else {
                    return Ok((false, None, None));
                };
                let extra: serde_json::Value = serde_json::from_str(&row.extra).map_err(|error| {
                    AppError::Internal(format!(
                        "conversation {target_id} has invalid extra JSON: {error}"
                    ))
                })?;
                if !extra.is_object() {
                    return Err(AppError::Internal(format!(
                        "conversation {target_id} extra must be a JSON object"
                    )));
                }
                extra.get("autowork").cloned()
            }
            AutoWorkTargetKind::Terminal => {
                let Some(driver) = &self.terminal_driver else {
                    return Ok((false, None, None));
                };
                match driver.read_autowork(parse_terminal_id(target_id)?).await? {
                    Some(s) => Some(serde_json::from_str(&s).map_err(|error| {
                        AppError::Internal(format!(
                            "terminal {target_id} has invalid autowork JSON: {error}"
                        ))
                    })?),
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
        let terminal_id = parse_terminal_id(terminal_id)?;
        let user_id = UserId::parse(user_id)
            .map_err(|error| AppError::Forbidden(format!("invalid caller identity: {error}")))?;
        let Some(driver) = &self.terminal_driver else {
            return Ok(());
        };
        let desc = driver
            .describe(terminal_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("terminal {terminal_id}")))?;
        let owner_id = UserId::parse(&desc.user_id).map_err(|error| {
            AppError::Forbidden(format!("terminal {terminal_id} has invalid owner: {error}"))
        })?;
        if owner_id != user_id {
            return Err(AppError::Forbidden(format!(
                "terminal {terminal_id} is not owned by the caller"
            )));
        }
        Ok(())
    }

    /// Ensure a terminal is eligible for AutoWork: it must be a verdict-capable
    /// agent CLI (one with a lifecycle-hook renderer —claude/codex, including
    /// wrappers like `stepcode claude` —those get the Stop —TurnEnd hook +
    /// requirement MCP injected) and currently running. `BadRequest` otherwise.
    ///
    /// Eligibility is resolved from the launch `(command, args, backend)` via
    /// `nomifun_terminal::terminal_autowork_capable`, the SAME logic the launch
    /// injector uses —so the gate never rejects a terminal the platform would
    /// actually hook (the historical bug: a custom/wrapper launch stored
    /// `backend = None` and was rejected despite being injectable).
    pub async fn ensure_terminal_autowork_eligible(&self, terminal_id: &str) -> Result<(), AppError> {
        let Some(driver) = &self.terminal_driver else {
            return Err(AppError::Internal("terminal driver not attached".into()));
        };
        let desc = driver
            .describe(parse_terminal_id(terminal_id)?)
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

    /// Called by the AutoWork runner after a turn ends. If the agent already moved
    /// the row to a terminal state (via its completion tool / terminal marker),
    /// respect it. Otherwise:
    /// - clean turn + `expects_verdict` —mark `needs_review` (the agent had a
    ///   way to declare done/failed but didn't, so we do NOT silently assume
    ///   success —a human verifies). This is the soft-failure guard.
    /// - clean turn + NOT `expects_verdict` —mark `done` (legacy: the engine has
    ///   no declaration channel, so a clean finish is the best signal we have).
    /// - error —if `attempt_count < MAX_ATTEMPTS` re-pend for retry, else mark
    ///   `failed` and pause the tag.
    ///
    /// `expects_verdict` is true when the engine WAS given an explicit way to
    /// declare the outcome (nomi native tools, ACP requirement MCP, terminal
    /// marker). Returns the final DTO (or None if the row vanished).
    pub async fn finalize_if_needed(
        &self,
        id: &str,
        turn_errored: bool,
        note: Option<String>,
        expects_verdict: bool,
    ) -> Result<Option<Requirement>, AppError> {
        let id = validate_requirement_id(id)?;
        let Some(row) = self.repo.get_by_requirement_id(id).await? else {
            return Ok(None);
        };
        // Agent already reached a terminal state itself —respect it (its own
        // note, e.g. from the nomi `requirement_complete` tool, wins).
        if row.status == "done" || row.status == "failed" || row.status == "cancelled" {
            return Ok(Some(row_to_dto(&row)));
        }
        // Still in_progress (or pending): decide based on the turn outcome.
        if !turn_errored {
            let note = note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
            if expects_verdict {
                // The agent could have declared done/failed but ended the turn
                // without doing so —ambiguous. Park for human review instead of
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
                owner_conversation_id: Some(None),
                owner_terminal_id: Some(None),

                active_turn_started_at: Some(None),
                lease_expires_at: Some(None),
                ..Default::default()
            };
            self.repo.update(id, &params).await?;
            let updated = self.repo.get_by_requirement_id(id).await?;
            // Back to pending —wake idle loops (this or a sibling session retries).
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
        // A requirement exhausted its retries —PAUSE the whole tag so the
        // AutoWork runner stops claiming the tag's remaining requirements until a
        // human resumes it. This is the fix for "a failed predecessor lets every
        // successor barge in instantly". Best-effort: a pause-write failure must
        // not mask the failed-status transition that already succeeded.
        match self.repo.pause_tag(&row.tag, "requirement_failed", Some(id), now_ms()).await {
            Ok(()) => self.emitter.emit_tag_paused(&nomifun_api_types::TagPausedPayload {
                tag: row.tag.clone(),
                reason: "requirement_failed".to_string(),
                requirement_id: Some(id.to_owned()),
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
    pub async fn resume_tag(&self, tag: &str, requeue_ids: &[String]) -> Result<(), AppError> {
        for id in requeue_ids {
            validate_requirement_id(id)?;
        }
        self.repo.resume_tag(tag).await?;
        for id in requeue_ids {
            // Only re-pend rows that are genuinely failed AND belong to this tag.
            let Some(row) = self.repo.get_by_requirement_id(id).await? else { continue };
            if row.tag != tag || row.status != "failed" {
                continue;
            }
            let params = RequirementRowUpdate {
                status: Some("pending".to_string()),
                completion_note: Some(None),
                owner_conversation_id: Some(None),
                owner_terminal_id: Some(None),

                active_turn_started_at: Some(None),
                lease_expires_at: Some(None),
                attempt_count: Some(0),
                ..Default::default()
            };
            self.repo.update(id, &params).await?;
            if let Some(updated) = self.repo.get_by_requirement_id(id).await? {
                self.emitter.emit_status_changed(&row_to_dto(&updated));
            }
        }
        // Tag is active again (+ any requeued rows are pending) —wake idle loops.
        self.wake_autowork();
        Ok(())
    }

    /// Resume a tag because AutoWork was explicitly (re-)ENABLED on a session
    /// bound to it. A paused tag (prior `requirement_failed`, or a deleted-session
    /// cascade) otherwise silently blocks EVERY conversation bound to the same tag
    /// —the user toggles AutoWork on and nothing happens, with no per-conversation
    /// indication that the shared tag is paused (the recurring "nothing runs"
    /// trap).
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
                owner_conversation_id: Some(None),
                owner_terminal_id: Some(None),

                active_turn_started_at: Some(None),
                lease_expires_at: Some(None),
                attempt_count: Some(0),
                ..Default::default()
            };
            self.repo.update(&row.requirement_id, &params).await?;
            if let Some(updated) = self
                .repo
                .get_by_requirement_id(&row.requirement_id)
                .await?
            {
                self.emitter.emit_status_changed(&row_to_dto(&updated));
            }
        }
        self.wake_autowork();
        Ok(())
    }


    /// WITHOUT consuming an attempt (the turn never ran). Wakes loops to retry.
    pub async fn unclaim_busy(
        &self,
        id: &str,
        owner_id: &str,
        kind: AutoWorkTargetKind,
    ) -> Result<(), AppError> {
        let id = validate_requirement_id(id)?;
        let owners = match kind {
            AutoWorkTargetKind::Conversation => (Some(parse_conversation_id(owner_id)?), None),
            AutoWorkTargetKind::Terminal => (None, Some(parse_terminal_id(owner_id)?)),
        };
        if self.repo.unclaim(id, owners.0, owners.1).await? {
            if let Some(updated) = self.repo.get_by_requirement_id(id).await? {
                self.emitter.emit_status_changed(&row_to_dto(&updated));
            }
            self.wake_autowork();
        }
        Ok(())
    }

    /// §9.B — clear the owner of every requirement bound to a now-deleted
    /// session, unblocking requirements whose conversation/terminal UUIDv7
    /// executing session was deleted.
    ///
    /// The owner columns intentionally have no cross-table FK, so a deleted
    /// conversation/terminal does not cascade-clear them. Without this hook a requirement claimed by a
    /// since-deleted session would keep a dangling owner and, if it was
    /// `in_progress`, sit orphaned until the lease sweeper happened to run.
    ///
    /// Clears both owner columns together (paired-NULL CHECK), and re-pends any
    /// `in_progress` row (its session is gone, so it can never finish) WITHOUT
    /// consuming an attempt —the session vanishing is not a failed attempt.
    /// Idempotent; wakes idle loops so a re-pended requirement is reclaimable now.
    ///
    /// CALL SITE (Phase 3/4 wiring): invoke from the conversations + terminal
    /// deletion paths (`nomifun-conversation` / `nomifun-terminal` delete) with
    /// the deleted session id AND its domain. Exposed here so the deletion path
    /// can call it without the DB layer (the FK that would have cascaded does
    /// not exist).
    ///
    /// SECURITY (spec §2.2): the query is scoped to the owner column for the
    /// requested domain. Clearing a conversation can never release terminal
    /// work, and vice versa.
    pub async fn clear_owner_for_session(
        &self,
        session_id: &str,
        kind: AutoWorkTargetKind,
    ) -> Result<u64, AppError> {
        let session_id = match kind {
            AutoWorkTargetKind::Conversation => parse_conversation_id(session_id)?,
            AutoWorkTargetKind::Terminal => parse_terminal_id(session_id)?,
        };
        // Page through every requirement owned by this session (paired domain).
        let mut cleared = 0u64;
        let mut woke = false;
        loop {
            let params = ListRequirementsParams {
                owner_conversation_id: (kind == AutoWorkTargetKind::Conversation)
                    .then(|| session_id.to_owned()),
                owner_terminal_id: (kind == AutoWorkTargetKind::Terminal)
                    .then(|| session_id.to_owned()),
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
                    owner_conversation_id: Some(None),
                    owner_terminal_id: Some(None),
                    ..Default::default()
                };
                if re_pend {
                    update.status = Some("pending".to_string());
                    update.active_turn_started_at = Some(None);
                    update.lease_expires_at = Some(None);
                    woke = true;
                }
                self.repo.update(&row.requirement_id, &update).await?;
                if let Some(updated) = self
                    .repo
                    .get_by_requirement_id(&row.requirement_id)
                    .await?
                {
                    self.emitter.emit_status_changed(&row_to_dto(&updated));
                }
                cleared += 1;
            }
            // The list query filters on the selected owner-domain column; cleared
            // rows drop out of the result set, so re-querying page 1 always
            // advances. Guard against a short page (no more rows) to terminate.
            if batch < 200 {
                break;
            }
        }
        if woke {
            self.wake_autowork();
        }
        Ok(cleared)
    }

    /// Enumerate AutoWork tag/session bindings for `user_id`, grouped by tag.
    ///
    /// A "binding" is a conversation or terminal whose persisted AutoWork config
    /// is `enabled`, pointing at a tag. The `run_state` returned here reflects the
    /// persisted config only (`Idle` for every enabled binding); the routes layer
    /// upgrades it to `Active` for targets the AutoWork runner is currently driving
    /// (it owns the live progress map). Used by the AutoWork admin session bindings tab.
    pub async fn tag_bindings(&self, user_id: &str) -> Result<Vec<TagBindings>, AppError> {
        let user_id = UserId::parse(user_id)
            .map_err(|error| AppError::Forbidden(format!("invalid caller identity: {error}")))?;
        let user_id = user_id.as_str();
        // (tag, binding) accumulator, grouped at the end.
        let mut by_tag: std::collections::BTreeMap<String, Vec<TagBinding>> = std::collections::BTreeMap::new();

        // Conversations: page through all of the user's conversations and parse
        // each `extra.autowork` directly from
        // the row we already hold (no extra per-row query).
        if let Some(conv_repo) = &self.conversation_repo {
            let mut cursor: Option<String> = None;
            loop {
                let filters = ConversationFilters {
                    cursor: cursor.clone(),
                    limit: 200,
                    source: None,
                    cron_job_id: None,
                    pinned: None,
                    exclude_companion_companion: false,
                    // Keep unrelated conversation filters at their defaults.
                    ..Default::default()
                };
                let page = conv_repo.list_paginated(user_id, &filters).await?;
                if page.items.is_empty() {
                    break;
                }
                for row in &page.items {
                    let extra: serde_json::Value = serde_json::from_str(&row.extra).map_err(|error| {
                        AppError::Internal(format!(
                            "conversation {} has invalid extra JSON: {error}",
                            row.conversation_id
                        ))
                    })?;
                    if !extra.is_object() {
                        return Err(AppError::Internal(format!(
                            "conversation {} extra must be a JSON object",
                            row.conversation_id
                        )));
                    }
                    let aw = extra.get("autowork");
                    let Some(aw) = aw else { continue };
                    let enabled = aw.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                    let tag = aw.get("tag").and_then(|v| v.as_str()).map(|s| s.to_string());
                    if let (true, Some(tag)) = (enabled, tag) {
                        by_tag.entry(tag).or_default().push(TagBinding {
                            kind: AutoWorkTargetKind::Conversation,
                            target_id: row.conversation_id.clone(),
                            name: if row.name.is_empty() {
                                row.conversation_id.clone()
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
                cursor = page.items.last().map(|r| r.conversation_id.clone());
            }
        }

        // Terminals: enumerate the user's sessions and parse the `autowork` column.
        if let Some(term_repo) = &self.terminal_repo {
            for row in term_repo.list_by_user(user_id).await? {
                let Some(blob) = row.autowork.as_deref() else { continue };
                let aw: serde_json::Value = serde_json::from_str(blob).map_err(|error| {
                    AppError::Internal(format!(
                        "terminal {} has invalid autowork JSON: {error}",
                        row.terminal_id
                    ))
                })?;
                let enabled = aw.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                let tag = aw.get("tag").and_then(|v| v.as_str()).map(|s| s.to_string());
                if let (true, Some(tag)) = (enabled, tag) {
                    by_tag.entry(tag).or_default().push(TagBinding {
                        kind: AutoWorkTargetKind::Terminal,
                        target_id: row.terminal_id.to_string(),
                        name: if row.name.is_empty() {
                            row.terminal_id.to_string()
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

/// Conversation-delete hook (spec §9.B): when an owning conversation UUIDv7 is
/// deleted, clear the conversation owner of every requirement it owned. There
/// is no FK cascade, so the deletion path drives this explicitly. Wired in `nomifun-app` via
/// `ConversationService::with_delete_hook`.
#[async_trait::async_trait]
impl nomifun_common::OnConversationDelete for RequirementService {
    async fn on_conversation_deleted(&self, _user_id: &str, conversation_id: &str) {
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
/// terminal owner domain. Wired in `nomifun-app` via
/// `TerminalService::with_delete_hook`.
#[async_trait::async_trait]
impl nomifun_common::OnTerminalDelete for RequirementService {
    async fn on_terminal_deleted(&self, _user_id: &str, terminal_id: &str) {
        if let Err(e) = self
            .clear_owner_for_session(&terminal_id, AutoWorkTargetKind::Terminal)
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
    use nomifun_common::{ConversationId, TerminalId, UserId};
    use nomifun_db::{
        IAttachmentRepository, SqliteAttachmentRepository, SqliteRequirementRepository,
        init_database_memory,
    };
    use nomifun_realtime::UserEventSink;

    #[derive(Default)]
    struct NoopBroadcaster;
    impl UserEventSink for NoopBroadcaster {
        fn send_to_user(
            &self,
            _user_id: &str,
            _event: nomifun_api_types::WebSocketMessage<serde_json::Value>,
        ) {
        }
    }

    async fn service_with_owners() -> (RequirementService, String, String) {
        let db = init_database_memory().await.unwrap();
        let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
        let repo: Arc<dyn IRequirementRepository> =
            Arc::new(SqliteRequirementRepository::new(db.pool().clone()));
        let conversation_id = ConversationId::new().into_string();
        let terminal_id = TerminalId::new().into_string();
        sqlx::query(
            "INSERT INTO conversations \
                (conversation_id, user_id, name, type, created_at, updated_at) \
             VALUES (?1, ?2, 'Requirement Conversation', 'nomi', 0, 0)",
        )
        .bind(&conversation_id)
        .bind(&installation_owner)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO terminal_sessions \
                (terminal_id, user_id, name, cwd, command, args, cols, rows, last_status, created_at, updated_at) \
             VALUES (?1, ?2, 'Requirement Terminal', '/tmp', '$SHELL', '[]', 80, 24, 'running', 0, 0)",
        )
        .bind(&terminal_id)
        .bind(&installation_owner)
        .execute(db.pool())
        .await
        .unwrap();
        let emitter = RequirementEventEmitter::new(
            Arc::new(NoopBroadcaster),
            Arc::from(installation_owner.as_str()),
        );
        let service = RequirementService::new(repo, emitter);
        Box::leak(Box::new(db));
        (service, conversation_id, terminal_id)
    }

    async fn create_req(service: &RequirementService, tag: &str) -> Requirement {
        service
            .create(CreateRequirementRequest {
                title: "Do X".into(),
                content: "body".into(),
                tag: tag.into(),
                order_key: Some("1".into()),
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap()
    }

    async fn exhaust_requirement(
        service: &RequirementService,
        requirement_id: &str,
        tag: &str,
        conversation_id: &str,
    ) {
        for _ in 0..MAX_ATTEMPTS {
            service
                .claim_next(
                    tag,
                    conversation_id,
                    AutoWorkTargetKind::Conversation,
                    DEFAULT_LEASE_MS,
                )
                .await
                .unwrap()
                .expect("requirement remains claimable until retries are exhausted");
            service
                .finalize_if_needed(requirement_id, true, None, false)
                .await
                .unwrap();
        }
    }

    async fn service_with_attachments() -> (RequirementService, tempfile::TempDir, tempfile::TempDir) {
        let db = init_database_memory().await.unwrap();
        let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
        let repo: Arc<dyn IRequirementRepository> =
            Arc::new(SqliteRequirementRepository::new(db.pool().clone()));
        let attachment_repo: Arc<dyn IAttachmentRepository> =
            Arc::new(SqliteAttachmentRepository::new(db.pool().clone()));
        let emitter = RequirementEventEmitter::new(
            Arc::new(NoopBroadcaster),
            Arc::from(installation_owner.as_str()),
        );
        Box::leak(Box::new(db));

        let data_dir = tempfile::tempdir().unwrap();
        let upload_root = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(data_dir.path().to_path_buf(), attachment_repo)
            .with_upload_root(upload_root.path().to_path_buf());
        let service = RequirementService::new(repo, emitter).with_attachment_store(Arc::new(store));
        (service, data_dir, upload_root)
    }

    fn upload_file(root: &std::path::Path, name: &str) -> String {
        let path = root.join(name);
        std::fs::write(&path, b"test image bytes").unwrap();
        path.to_string_lossy().into_owned()
    }

    fn attachment_ref(source_path: String, file_name: &str) -> nomifun_api_types::NewAttachmentRef {
        nomifun_api_types::NewAttachmentRef {
            source_path,
            file_name: file_name.to_string(),
        }
    }

    #[tokio::test]
    async fn create_with_attachments_binds_and_returns_dtos() {
        let (service, data_dir, upload_root) = service_with_attachments().await;
        let created = service
            .create(CreateRequirementRequest {
                title: "With image".into(),
                content: String::new(),
                tag: "attachments".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![attachment_ref(upload_file(upload_root.path(), "a.png"), "a.png")],
            })
            .await
            .unwrap();

        assert!(RequirementId::parse(&created.requirement_id).is_ok());
        assert_eq!(created.attachments.len(), 1);
        assert_eq!(created.attachments[0].file_name, "a.png");
        assert!(std::path::Path::new(&created.attachments[0].abs_path).exists());
        assert!(
            created.attachments[0]
                .abs_path
                .starts_with(data_dir.path().to_string_lossy().as_ref())
        );
        assert_eq!(
            service
                .get(&created.requirement_id)
                .await
                .unwrap()
                .attachments
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn create_with_bad_attachment_rolls_back_requirement() {
        let (service, _data_dir, upload_root) = service_with_attachments().await;
        let error = service
            .create(CreateRequirementRequest {
                title: "Bad image".into(),
                content: String::new(),
                tag: "attachments".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![attachment_ref(upload_file(upload_root.path(), "bad.txt"), "bad.txt")],
            })
            .await
            .unwrap_err();

        assert!(matches!(error, AppError::BadRequest(_)));
        assert_eq!(
            service
                .list(&ListRequirementsQuery::default())
                .await
                .unwrap()
                .total,
            0,
            "a failed attachment ingest must roll back the canonical requirement row"
        );
    }

    #[tokio::test]
    async fn update_adds_and_removes_attachments() {
        let (service, _data_dir, upload_root) = service_with_attachments().await;
        let created = service
            .create(CreateRequirementRequest {
                title: "Replace image".into(),
                content: String::new(),
                tag: "attachments".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![attachment_ref(upload_file(upload_root.path(), "a.png"), "a.png")],
            })
            .await
            .unwrap();
        let removed_path = created.attachments[0].abs_path.clone();
        let removed_id = created.attachments[0].attachment_id.clone();
        let updated = service
            .update(
                &created.requirement_id,
                UpdateRequirementRequest {
                    title: None,
                    content: None,
                    tag: None,
                    order_key: None,
                    status: None,
                    completion_note: None,
                    add_attachments: vec![attachment_ref(
                        upload_file(upload_root.path(), "b.png"),
                        "b.png",
                    )],
                    remove_attachment_ids: vec![removed_id],
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.attachments.len(), 1);
        assert_eq!(updated.attachments[0].file_name, "b.png");
        assert!(!std::path::Path::new(&removed_path).exists());
    }

    #[tokio::test]
    async fn update_failed_ingest_preserves_removal_targets() {
        let (service, _data_dir, upload_root) = service_with_attachments().await;
        let created = service
            .create(CreateRequirementRequest {
                title: "Atomic image update".into(),
                content: String::new(),
                tag: "attachments".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![attachment_ref(upload_file(upload_root.path(), "a.png"), "a.png")],
            })
            .await
            .unwrap();
        let original = created.attachments[0].clone();
        let error = service
            .update(
                &created.requirement_id,
                UpdateRequirementRequest {
                    title: None,
                    content: None,
                    tag: None,
                    order_key: None,
                    status: None,
                    completion_note: None,
                    add_attachments: vec![attachment_ref(
                        upload_file(upload_root.path(), "bad.txt"),
                        "bad.txt",
                    )],
                    remove_attachment_ids: vec![original.attachment_id.clone()],
                },
            )
            .await
            .unwrap_err();

        assert!(matches!(error, AppError::BadRequest(_)));
        let after = service.get(&created.requirement_id).await.unwrap();
        assert_eq!(after.attachments.len(), 1);
        assert_eq!(
            after.attachments[0].attachment_id,
            original.attachment_id
        );
        assert!(std::path::Path::new(&original.abs_path).exists());
    }

    #[tokio::test]
    async fn attachment_only_update_bumps_updated_at() {
        let (service, _data_dir, upload_root) = service_with_attachments().await;
        let created = service
            .create(CreateRequirementRequest {
                title: "Timestamp image update".into(),
                content: String::new(),
                tag: "attachments".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![attachment_ref(upload_file(upload_root.path(), "a.png"), "a.png")],
            })
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let updated = service
            .update(
                &created.requirement_id,
                UpdateRequirementRequest {
                    title: None,
                    content: None,
                    tag: None,
                    order_key: None,
                    status: None,
                    completion_note: None,
                    add_attachments: vec![attachment_ref(
                        upload_file(upload_root.path(), "b.png"),
                        "b.png",
                    )],
                    remove_attachment_ids: vec![],
                },
            )
            .await
            .unwrap();

        assert!(updated.updated_at > created.updated_at);
    }

    #[tokio::test]
    async fn delete_cleans_attachment_rows_and_files() {
        let (service, data_dir, upload_root) = service_with_attachments().await;
        let created = service
            .create(CreateRequirementRequest {
                title: "Delete image".into(),
                content: String::new(),
                tag: "attachments".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![attachment_ref(upload_file(upload_root.path(), "a.png"), "a.png")],
            })
            .await
            .unwrap();
        let requirement_id = created.requirement_id;
        service.delete(&requirement_id).await.unwrap();

        assert!(!data_dir.path().join("attachments").join(&requirement_id).exists());
    }

    #[tokio::test]
    async fn create_get_update_list_and_delete_use_business_ids() {
        let (service, _conversation_id, _terminal_id) = service_with_owners().await;
        let req = create_req(&service, "alpha").await;
        assert!(RequirementId::parse(&req.requirement_id).is_ok());
        assert_eq!(
            service.get(&req.requirement_id).await.unwrap().requirement_id,
            req.requirement_id
        );

        let updated = service
            .update(
                &req.requirement_id,
                UpdateRequirementRequest {
                    title: Some("Updated".into()),
                    content: None,
                    tag: None,
                    order_key: None,
                    status: None,
                    completion_note: None,
                    add_attachments: vec![],
                    remove_attachment_ids: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.title, "Updated");
        let page = service
            .list(&ListRequirementsQuery {
                tag: Some("alpha".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 1);

        service.delete(&req.requirement_id).await.unwrap();
        assert!(matches!(
            service.get(&req.requirement_id).await.unwrap_err(),
            AppError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn conversation_and_terminal_claims_are_domain_scoped() {
        let (service, conversation_id, terminal_id) = service_with_owners().await;
        let conversation_req = create_req(&service, "conv").await;
        let terminal_req = create_req(&service, "term").await;

        let claimed = service
            .claim_next(
                "conv",
                &conversation_id,
                AutoWorkTargetKind::Conversation,
                DEFAULT_LEASE_MS,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            claimed.owner_conversation_id.as_deref(),
            Some(conversation_id.as_str())
        );
        assert!(claimed.owner_terminal_id.is_none());
        assert_eq!(claimed.requirement_id, conversation_req.requirement_id);

        let term_claimed = service
            .claim_next(
                "term",
                &terminal_id,
                AutoWorkTargetKind::Terminal,
                DEFAULT_LEASE_MS,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            term_claimed.owner_terminal_id.as_deref(),
            Some(terminal_id.as_str())
        );
        assert!(term_claimed.owner_conversation_id.is_none());
        assert_eq!(term_claimed.requirement_id, terminal_req.requirement_id);

        assert!(
            !service
                .renew_lease(
                    &terminal_req.requirement_id,
                    &conversation_id,
                    AutoWorkTargetKind::Conversation,
                    DEFAULT_LEASE_MS,
                )
                .await
                .unwrap(),
            "wrong owner domain cannot renew terminal claim"
        );
        assert!(
            service
                .renew_lease(
                    &terminal_req.requirement_id,
                    &terminal_id,
                    AutoWorkTargetKind::Terminal,
                    DEFAULT_LEASE_MS,
                )
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn finalize_transitions_clean_error_and_exhaustion() {
        let (service, conversation_id, _terminal_id) = service_with_owners().await;
        let clean = create_req(&service, "clean").await;
        service
            .claim_next(
                "clean",
                &conversation_id,
                AutoWorkTargetKind::Conversation,
                DEFAULT_LEASE_MS,
            )
            .await
            .unwrap();
        let done = service
            .finalize_if_needed(
                &clean.requirement_id,
                false,
                Some("finished".into()),
                false,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.status, RequirementStatus::Done);
        assert_eq!(done.completion_note.as_deref(), Some("finished"));

        let review = create_req(&service, "review").await;
        service
            .claim_next(
                "review",
                &conversation_id,
                AutoWorkTargetKind::Conversation,
                DEFAULT_LEASE_MS,
            )
            .await
            .unwrap();
        let parked = service
            .finalize_if_needed(&review.requirement_id, false, None, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(parked.status, RequirementStatus::NeedsReview);

        let retry = create_req(&service, "retry").await;
        service
            .claim_next(
                "retry",
                &conversation_id,
                AutoWorkTargetKind::Conversation,
                DEFAULT_LEASE_MS,
            )
            .await
            .unwrap();
        let pending = service
            .finalize_if_needed(&retry.requirement_id, true, None, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.status, RequirementStatus::Pending);
        assert!(pending.owner_conversation_id.is_none());
        assert!(pending.owner_terminal_id.is_none());
    }

    #[tokio::test]
    async fn finalize_respects_agent_verdict_and_terminal_state_is_frozen() {
        let (service, conversation_id, _terminal_id) = service_with_owners().await;
        let requirement = create_req(&service, "agent-verdict").await;
        service
            .claim_next(
                "agent-verdict",
                &conversation_id,
                AutoWorkTargetKind::Conversation,
                DEFAULT_LEASE_MS,
            )
            .await
            .unwrap();
        service
            .complete(&requirement.requirement_id, Some("agent did it".into()))
            .await
            .unwrap();

        let finalized = service
            .finalize_if_needed(&requirement.requirement_id, false, None, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(finalized.status, RequirementStatus::Done);
        assert_eq!(finalized.completion_note.as_deref(), Some("agent did it"));
        assert!(matches!(
            service
                .set_status(
                    &requirement.requirement_id,
                    RequirementStatus::InProgress,
                    None,
                )
                .await
                .unwrap_err(),
            AppError::BadRequest(_)
        ));
        assert_eq!(
            service
                .set_status(&requirement.requirement_id, RequirementStatus::Done, None)
                .await
                .unwrap()
                .completion_note
                .as_deref(),
            Some("agent did it"),
            "an idempotent terminal-state write must retain the existing verdict"
        );
    }

    #[tokio::test]
    async fn a_no_note_verdict_clears_stale_review_prose() {
        let (service, _conversation_id, _terminal_id) = service_with_owners().await;
        let requirement = create_req(&service, "stale-note").await;
        service
            .set_status(
                &requirement.requirement_id,
                RequirementStatus::NeedsReview,
                Some("unable to declare a verdict".into()),
            )
            .await
            .unwrap();

        let done = service
            .set_status(&requirement.requirement_id, RequirementStatus::Done, None)
            .await
            .unwrap();
        assert_eq!(done.status, RequirementStatus::Done);
        assert_eq!(done.completion_note, None);
    }

    #[tokio::test]
    async fn needs_review_roundtrips_and_remains_human_resolvable() {
        let (service, conversation_id, _terminal_id) = service_with_owners().await;
        let requirement = create_req(&service, "reviewable").await;
        service
            .claim_next(
                "reviewable",
                &conversation_id,
                AutoWorkTargetKind::Conversation,
                DEFAULT_LEASE_MS,
            )
            .await
            .unwrap();
        let review = service
            .finalize_if_needed(
                &requirement.requirement_id,
                false,
                Some("please verify".into()),
                true,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(review.status, RequirementStatus::NeedsReview);
        assert_eq!(review.completion_note.as_deref(), Some("please verify"));
        assert!(!service.is_tag_paused("reviewable").await.unwrap());
        assert_eq!(service.board("reviewable").await.unwrap().needs_review.len(), 1);
        assert_eq!(
            service
                .tags()
                .await
                .unwrap()
                .into_iter()
                .find(|summary| summary.tag == "reviewable")
                .unwrap()
                .needs_review,
            1
        );
        assert_eq!(
            service
                .set_status(&requirement.requirement_id, RequirementStatus::Done, None)
                .await
                .unwrap()
                .status,
            RequirementStatus::Done
        );
    }

    #[tokio::test]
    async fn exhausted_retries_pause_tag_and_explicit_resume_requeues() {
        let (service, conversation_id, _terminal_id) = service_with_owners().await;
        let requirement = create_req(&service, "retry-pause").await;
        exhaust_requirement(
            &service,
            &requirement.requirement_id,
            "retry-pause",
            &conversation_id,
        )
        .await;

        let failed = service.get(&requirement.requirement_id).await.unwrap();
        assert_eq!(failed.status, RequirementStatus::Failed);
        assert_eq!(failed.attempt_count, MAX_ATTEMPTS);
        assert!(service.is_tag_paused("retry-pause").await.unwrap());
        let state = service.tag_state("retry-pause").await.unwrap().unwrap();
        assert_eq!(state.paused_reason.as_deref(), Some("requirement_failed"));
        assert_eq!(
            state.paused_requirement_id,
            Some(requirement.requirement_id.clone())
        );

        service
            .resume_tag(
                "retry-pause",
                std::slice::from_ref(&requirement.requirement_id),
            )
            .await
            .unwrap();
        let requeued = service.get(&requirement.requirement_id).await.unwrap();
        assert_eq!(requeued.status, RequirementStatus::Pending);
        assert_eq!(requeued.attempt_count, 0);
        assert!(!service.is_tag_paused("retry-pause").await.unwrap());
        assert!(
            service
                .claim_next(
                    "retry-pause",
                    &conversation_id,
                    AutoWorkTargetKind::Conversation,
                    DEFAULT_LEASE_MS,
                )
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn enable_resume_refreshes_paused_work_but_not_healthy_work() {
        let (service, conversation_id, _terminal_id) = service_with_owners().await;
        let stuck = create_req(&service, "enable-resume").await;
        exhaust_requirement(
            &service,
            &stuck.requirement_id,
            "enable-resume",
            &conversation_id,
        )
        .await;
        service.resume_tag_for_enable("enable-resume").await.unwrap();
        let refreshed = service.get(&stuck.requirement_id).await.unwrap();
        assert_eq!(refreshed.status, RequirementStatus::Pending);
        assert_eq!(refreshed.attempt_count, 0);
        assert!(!service.is_tag_paused("enable-resume").await.unwrap());

        let healthy = create_req(&service, "healthy-enable").await;
        service
            .claim_next(
                "healthy-enable",
                &conversation_id,
                AutoWorkTargetKind::Conversation,
                DEFAULT_LEASE_MS,
            )
            .await
            .unwrap();
        service
            .finalize_if_needed(&healthy.requirement_id, true, None, false)
            .await
            .unwrap();
        assert_eq!(
            service
                .get(&healthy.requirement_id)
                .await
                .unwrap()
                .attempt_count,
            1
        );
        service.resume_tag_for_enable("healthy-enable").await.unwrap();
        assert_eq!(
            service
                .get(&healthy.requirement_id)
                .await
                .unwrap()
                .attempt_count,
            1,
            "enabling an unpaused tag must not reset a healthy retry budget"
        );
    }

    #[tokio::test]
    async fn busy_unclaim_requeues_without_consuming_attempt() {
        let (service, conversation_id, _terminal_id) = service_with_owners().await;
        let requirement = create_req(&service, "busy").await;
        let claimed = service
            .claim_next(
                "busy",
                &conversation_id,
                AutoWorkTargetKind::Conversation,
                DEFAULT_LEASE_MS,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.attempt_count, 1);

        service
            .unclaim_busy(
                &requirement.requirement_id,
                &conversation_id,
                AutoWorkTargetKind::Conversation,
            )
            .await
            .unwrap();
        let requeued = service.get(&requirement.requirement_id).await.unwrap();
        assert_eq!(requeued.status, RequirementStatus::Pending);
        assert_eq!(requeued.attempt_count, 0);
    }

    #[tokio::test]
    async fn user_interrupt_pauses_then_resume_allows_reclaim() {
        let (service, conversation_id, _terminal_id) = service_with_owners().await;
        let requirement = create_req(&service, "interrupted").await;
        service
            .claim_next(
                "interrupted",
                &conversation_id,
                AutoWorkTargetKind::Conversation,
                DEFAULT_LEASE_MS,
            )
            .await
            .unwrap();
        service
            .user_interrupt(
                &requirement.requirement_id,
                &conversation_id,
                "interrupted",
            )
            .await
            .unwrap();

        let interrupted = service.get(&requirement.requirement_id).await.unwrap();
        assert_eq!(interrupted.status, RequirementStatus::Pending);
        assert_eq!(interrupted.attempt_count, 1);
        assert!(service.is_tag_paused("interrupted").await.unwrap());
        assert!(
            service
                .claim_next(
                    "interrupted",
                    &conversation_id,
                    AutoWorkTargetKind::Conversation,
                    DEFAULT_LEASE_MS,
                )
                .await
                .unwrap()
                .is_none()
        );

        service.resume_tag("interrupted", &[]).await.unwrap();
        assert!(
            service
                .claim_next(
                    "interrupted",
                    &conversation_id,
                    AutoWorkTargetKind::Conversation,
                    DEFAULT_LEASE_MS,
                )
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn clear_owner_is_scoped_to_domain_and_requeues_work() {
        let (service, conversation_id, terminal_id) = service_with_owners().await;
        let conv_req = create_req(&service, "conv").await;
        let term_req = create_req(&service, "term").await;
        service
            .claim_next(
                "conv",
                &conversation_id,
                AutoWorkTargetKind::Conversation,
                DEFAULT_LEASE_MS,
            )
            .await
            .unwrap();
        service
            .claim_next(
                "term",
                &terminal_id,
                AutoWorkTargetKind::Terminal,
                DEFAULT_LEASE_MS,
            )
            .await
            .unwrap();

        assert_eq!(
            service
                .clear_owner_for_session(&conversation_id, AutoWorkTargetKind::Conversation)
                .await
                .unwrap(),
            1
        );
        let conv_after = service.get(&conv_req.requirement_id).await.unwrap();
        assert_eq!(conv_after.status, RequirementStatus::Pending);
        assert!(conv_after.owner_conversation_id.is_none());
        let term_after = service.get(&term_req.requirement_id).await.unwrap();
        assert_eq!(
            term_after.owner_terminal_id.as_deref(),
            Some(terminal_id.as_str())
        );
    }

    #[tokio::test]
    async fn conversation_release_cannot_release_terminal_owned_work() {
        let (service, conversation_id, terminal_id) = service_with_owners().await;
        let terminal_requirement = create_req(&service, "terminal-release").await;
        service
            .claim_next(
                "terminal-release",
                &terminal_id,
                AutoWorkTargetKind::Terminal,
                DEFAULT_LEASE_MS,
            )
            .await
            .unwrap();

        service
            .release_claim(&terminal_requirement.requirement_id, &conversation_id)
            .await
            .unwrap();
        let terminal_after = service
            .get(&terminal_requirement.requirement_id)
            .await
            .unwrap();
        assert_eq!(terminal_after.status, RequirementStatus::InProgress);
        assert_eq!(
            terminal_after.owner_terminal_id.as_deref(),
            Some(terminal_id.as_str())
        );
        assert!(terminal_after.owner_conversation_id.is_none());

        let conversation_requirement = create_req(&service, "conversation-release").await;
        service
            .claim_next(
                "conversation-release",
                &conversation_id,
                AutoWorkTargetKind::Conversation,
                DEFAULT_LEASE_MS,
            )
            .await
            .unwrap();
        service
            .release_claim(&conversation_requirement.requirement_id, &conversation_id)
            .await
            .unwrap();
        let conversation_after = service
            .get(&conversation_requirement.requirement_id)
            .await
            .unwrap();
        assert_eq!(conversation_after.status, RequirementStatus::Pending);
        assert!(conversation_after.owner_conversation_id.is_none());
        assert!(conversation_after.owner_terminal_id.is_none());
    }

    struct MockDriver {
        user_id: String,
        command: String,
        args: Vec<String>,
        backend: Option<String>,
        last_status: String,
        exists: bool,
        autowork: std::sync::Mutex<Option<String>>,
        idmm: std::sync::Mutex<Option<String>>,
    }

    impl MockDriver {
        fn agent(user_id: String) -> Self {
            Self {
                user_id,
                command: String::new(),
                args: vec![],
                backend: Some("claude".into()),
                last_status: "running".into(),
                exists: true,
                autowork: std::sync::Mutex::new(None),
                idmm: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl TerminalDriver for MockDriver {
        async fn write_input(
            &self,
            _id: &str,
            _bytes: &[u8],
        ) -> Result<(), nomifun_terminal::error::TerminalError> {
            Ok(())
        }

        fn subscribe_output(
            &self,
            _id: &str,
        ) -> Option<tokio::sync::broadcast::Receiver<Vec<u8>>> {
            None
        }

        fn is_alive(&self, _id: &str) -> bool {
            self.last_status == "running"
        }

        async fn describe(
            &self,
            _id: &str,
        ) -> Result<Option<nomifun_terminal::TerminalDescription>, nomifun_terminal::error::TerminalError> {
            if !self.exists {
                return Ok(None);
            }
            Ok(Some(nomifun_terminal::TerminalDescription {
                user_id: self.user_id.clone(),
                cwd: String::new(),
                command: self.command.clone(),
                args: self.args.clone(),
                backend: self.backend.clone(),
                mode: None,
                last_status: self.last_status.clone(),
            }))
        }

        async fn read_autowork(
            &self,
            _id: &str,
        ) -> Result<Option<String>, nomifun_terminal::error::TerminalError> {
            Ok(self.autowork.lock().unwrap().clone())
        }

        async fn write_autowork(
            &self,
            _id: &str,
            autowork: Option<&str>,
        ) -> Result<(), nomifun_terminal::error::TerminalError> {
            *self.autowork.lock().unwrap() = autowork.map(str::to_owned);
            Ok(())
        }

        async fn read_idmm(
            &self,
            _id: &str,
        ) -> Result<Option<String>, nomifun_terminal::error::TerminalError> {
            Ok(self.idmm.lock().unwrap().clone())
        }

        async fn write_idmm(
            &self,
            _id: &str,
            idmm: Option<&str>,
        ) -> Result<(), nomifun_terminal::error::TerminalError> {
            *self.idmm.lock().unwrap() = idmm.map(str::to_owned);
            Ok(())
        }

        fn subscribe_lifecycle(
            &self,
            _id: &str,
        ) -> Option<tokio::sync::broadcast::Receiver<nomifun_terminal::TerminalLifecycleEvent>> {
            None
        }
    }

    async fn service_with_driver(driver: Arc<dyn TerminalDriver>) -> RequirementService {
        let db = init_database_memory().await.unwrap();
        let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
        let repo: Arc<dyn IRequirementRepository> =
            Arc::new(SqliteRequirementRepository::new(db.pool().clone()));
        let emitter = RequirementEventEmitter::new(
            Arc::new(NoopBroadcaster),
            Arc::from(installation_owner.as_str()),
        );
        Box::leak(Box::new(db));
        RequirementService::new(repo, emitter).with_terminal_driver(driver)
    }

    #[tokio::test]
    async fn terminal_config_roundtrips_with_canonical_id() {
        let user_id = UserId::new().into_string();
        let terminal_id = TerminalId::new().into_string();
        let service = service_with_driver(Arc::new(MockDriver::agent(user_id))).await;

        service
            .save_autowork_config(
                AutoWorkTargetKind::Terminal,
                &terminal_id,
                true,
                Some("alpha"),
                Some(5),
            )
            .await
            .unwrap();
        let (enabled, tag, max) = service
            .read_autowork_config(AutoWorkTargetKind::Terminal, &terminal_id)
            .await
            .unwrap();
        assert!(enabled);
        assert_eq!(tag.as_deref(), Some("alpha"));
        assert_eq!(max, Some(5));
    }

    #[tokio::test]
    async fn verify_terminal_owner_enforces_isolation() {
        let owner_id = UserId::new().into_string();
        let terminal_id = TerminalId::new().into_string();
        let service = service_with_driver(Arc::new(MockDriver::agent(owner_id.clone()))).await;

        service
            .verify_terminal_owner(&terminal_id, &owner_id)
            .await
            .unwrap();
        let intruder_id = UserId::new().into_string();
        assert!(matches!(
            service
                .verify_terminal_owner(&terminal_id, &intruder_id)
                .await
                .unwrap_err(),
            AppError::Forbidden(_)
        ));

        let missing = Arc::new(MockDriver {
            exists: false,
            ..MockDriver::agent(owner_id.clone())
        });
        let missing_service = service_with_driver(missing).await;
        let missing_terminal_id = TerminalId::new().into_string();
        assert!(matches!(
            missing_service
                .verify_terminal_owner(&missing_terminal_id, &owner_id)
                .await
                .unwrap_err(),
            AppError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn terminal_autowork_eligibility_gates_backend_status_and_wrappers() {
        let owner_id = UserId::new().into_string();
        let terminal_id = TerminalId::new().into_string();
        service_with_driver(Arc::new(MockDriver::agent(owner_id.clone())))
            .await
            .ensure_terminal_autowork_eligible(&terminal_id)
            .await
            .unwrap();

        let plain_shell = Arc::new(MockDriver {
            backend: None,
            ..MockDriver::agent(owner_id.clone())
        });
        assert!(matches!(
            service_with_driver(plain_shell)
                .await
                .ensure_terminal_autowork_eligible(&terminal_id)
                .await
                .unwrap_err(),
            AppError::BadRequest(_)
        ));

        let exited = Arc::new(MockDriver {
            last_status: "exited".into(),
            ..MockDriver::agent(owner_id.clone())
        });
        assert!(matches!(
            service_with_driver(exited)
                .await
                .ensure_terminal_autowork_eligible(&terminal_id)
                .await
                .unwrap_err(),
            AppError::BadRequest(_)
        ));

        let unsupported = Arc::new(MockDriver {
            backend: Some("gemini".into()),
            ..MockDriver::agent(owner_id.clone())
        });
        assert!(matches!(
            service_with_driver(unsupported)
                .await
                .ensure_terminal_autowork_eligible(&terminal_id)
                .await
                .unwrap_err(),
            AppError::BadRequest(_)
        ));

        for (command, args) in [
            ("stepcode", vec!["claude"]),
            ("npx", vec!["codex"]),
            ("claude", vec!["--dangerously-skip-permissions"]),
        ] {
            let wrapper = Arc::new(MockDriver {
                command: command.into(),
                args: args.into_iter().map(str::to_owned).collect(),
                backend: None,
                ..MockDriver::agent(owner_id.clone())
            });
            service_with_driver(wrapper)
                .await
                .ensure_terminal_autowork_eligible(&terminal_id)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn invalid_ids_fail_closed_at_service_boundaries() {
        let (service, _conversation_id, _terminal_id) = service_with_owners().await;
        assert!(matches!(
            service
                .claim_next("x", "1", AutoWorkTargetKind::Conversation, DEFAULT_LEASE_MS)
                .await
                .unwrap_err(),
            AppError::NotFound(_)
        ));
        assert!(matches!(
            service
                .claim_next("x", "1", AutoWorkTargetKind::Terminal, DEFAULT_LEASE_MS)
                .await
                .unwrap_err(),
            AppError::NotFound(_)
        ));
    }
}

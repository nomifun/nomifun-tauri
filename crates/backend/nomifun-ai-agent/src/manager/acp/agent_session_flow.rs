use crate::manager::acp::AcpAgentManager;
use crate::runtime_state::{AgentRuntimeState, AgentRuntimeTurn};
use crate::manager::acp::mode_normalize::agent_metadata_uses_meta_resume;
use crate::protocol::events::{
    AgentStreamEvent, AvailableCommandsEventData, ErrorEventData, SessionAssignedEventData, StartEventData,
    TurnStopReason,
};
use crate::session::SessionId as DomainSessionId;
use crate::types::SendMessageData;
use agent_client_protocol::schema::{ContentBlock, LoadSessionRequest, PromptRequest, SessionId, StopReason};
use nomifun_api_types::{
    AgentErrorCode, AgentErrorOwnership, AgentErrorResolution, AgentErrorResolutionKind,
};
use nomifun_common::AppError;
use serde_json::Value;

use super::agent::sdk_to_snake_value;
use tracing::warn;

/// True when an `AppError` originates from an ACP `SessionNotFound`
/// reply. Used to decide whether `open_session_resume` should drop a
/// stale sid and fall through to `open_session_new` instead of
/// surfacing the error. The `AcpError::SessionNotFound -> AppError`
/// converter renders as "Session not found: <sid>", so we match on
/// that text rather than `AppError::NotFound` alone — other 404 paths
/// (e.g. "Workspace not found") must not trigger a session rebuild.
fn is_session_not_found(err: &AppError) -> bool {
    matches!(err, AppError::NotFound(msg) if msg.starts_with("Session not found"))
}

impl AcpAgentManager {
    /// Establish a fresh ACP session (session/new) and apply desired
    /// mode/model/config via reconcile. Does NOT send a prompt and
    /// does NOT emit Start/Finish — callers wrap that around if needed.
    ///
    /// Returns the CLI-assigned session id.
    pub(super) async fn open_session_new(&self) -> Result<String, AppError> {
        let req = self.params.new_session_request();
        let session_response = self.protocol.new_session(req).await.map_err(AppError::from)?;

        let sid = session_response.session_id.to_string();

        {
            let mut session = self.session.write().await;
            if let Some(models) = session_response.models {
                session.apply_advertised_models(models);
            }
            if let Some(modes) = session_response.modes {
                session.apply_advertised_modes(modes);
            }
            if let Some(config_options) = session_response.config_options {
                session.apply_advertised_config_options(config_options);
            }
            session.set_session_id(DomainSessionId::new(sid.clone()));
            // Mark that the next prompt should carry the first-prompt prelude
            // (preset_context + skill index). Consumed by SessionNewPreludeHook.
            session.mark_pending_session_new_prelude();
            // Knowledge retrieval-protocol section rides every session open.
            session.mark_pending_knowledge_prelude();
            self.commit_session_changes(&mut session).await;
        }
        self.emit_snapshot_events().await;

        // Notify session_sync consumer so the new id hits the DB and
        // future rebuilds can take the resume path.
        self.runtime
            .emit(AgentStreamEvent::SessionAssigned(SessionAssignedEventData {
                session_id: sid.clone(),
            }));

        // Best-effort reconcile on a freshly-opened session. SessionNotFound
        // here would be pathological (we just created the session) but is
        // still surfaced for consistency.
        self.reconcile_session(&sid).await?;
        Ok(sid)
    }

    /// Drop the in-aggregate session id and re-run `open_session_new`.
    /// Used as the rescue path when resume helpers see `SessionNotFound`.
    /// Emits a `warn!` so ops can still see the original failure that
    /// triggered the rebuild.
    async fn rebuild_after_session_not_found(&self, stale_sid: &str, err: &AppError) -> Result<String, AppError> {
        warn!(
            conversation_id = %self.params.conversation_id,
            stale_session_id = %stale_sid,
            error = %err,
            "open_session_resume: stale session id rejected by CLI; rebuilding via session/new"
        );
        {
            let mut session = self.session.write().await;
            session.clear_session_id();
            self.commit_session_changes(&mut session).await;
        }
        self.open_session_new().await
    }

    /// Resume an existing ACP session and apply desired mode/model/config.
    /// Does NOT send a prompt. Returns the (possibly rewritten) session id.
    ///
    /// - Claude-meta-resume backends: `session/new` with
    ///   `_meta.claudeCode.options.resume`. The CLI may assign a new session id,
    ///   which we persist via `SessionAssigned`.
    /// - `session/load`-capable backends (e.g. Codex, OpenCode): `session/load`,
    ///   keep id.
    /// - Backends that support neither: seed the aggregate and hope the CLI
    ///   still recognises the id (legacy behaviour — matches pre-refactor).
    ///
    /// In all three branches a `SessionNotFound` reply (the persisted sid
    /// became stale, e.g. after a CLI upgrade or restart) triggers
    /// `rebuild_after_session_not_found`, which clears the sid and
    /// re-runs `open_session_new`. ELECTRON-1HQ regressed because we
    /// silently swallowed this case during warmup, leaving every
    /// subsequent `session/prompt` to surface the same error to the user.
    pub(super) async fn open_session_resume(&self, session_id: &str) -> Result<String, AppError> {
        if agent_metadata_uses_meta_resume(&self.params.metadata) {
            let mut meta = serde_json::Map::new();
            let mut claude_code = serde_json::Map::new();
            let mut options = serde_json::Map::new();
            options.insert("resume".into(), Value::String(session_id.to_owned()));
            claude_code.insert("options".into(), Value::Object(options));
            meta.insert("claudeCode".into(), Value::Object(claude_code));

            let req = self.params.new_session_request().meta(meta);
            let new_response = match self.protocol.new_session(req).await.map_err(AppError::from) {
                Ok(r) => r,
                Err(e) if is_session_not_found(&e) => {
                    return self.rebuild_after_session_not_found(session_id, &e).await;
                }
                Err(e) => return Err(e),
            };
            let new_sid = new_response.session_id.to_string();

            {
                let mut session = self.session.write().await;
                if let Some(models) = new_response.models {
                    session.apply_advertised_models(models);
                }
                if let Some(modes) = new_response.modes {
                    session.apply_advertised_modes(modes);
                }
                if let Some(config_options) = new_response.config_options {
                    session.apply_advertised_config_options(config_options);
                }
                session.set_session_id(DomainSessionId::new(new_sid.clone()));
                // This process/runtime activation must receive the immutable
                // preset contract even when the underlying CLI session resumes.
                session.mark_pending_session_new_prelude();
                session.mark_pending_knowledge_prelude();
                self.commit_session_changes(&mut session).await;
            }
            self.emit_snapshot_events().await;

            if new_sid != session_id {
                self.runtime
                    .emit(AgentStreamEvent::SessionAssigned(SessionAssignedEventData {
                        session_id: new_sid.clone(),
                    }));
            }

            return match self.reconcile_session(&new_sid).await {
                Ok(()) => Ok(new_sid),
                Err(e) if is_session_not_found(&e) => self.rebuild_after_session_not_found(&new_sid, &e).await,
                Err(e) => Err(e),
            };
        }

        let (supports_load, preloaded_mode) = {
            let session = self.session.read().await;
            (
                session.agent_capabilities().map(|c| c.load_session).unwrap_or(false),
                session.modes().map(|m| m.current_mode_id.to_string()),
            )
        };

        if supports_load {
            let mut load_req = LoadSessionRequest::new(SessionId::new(session_id), &self.params.workspace.path);
            if !self.params.mcp_servers.is_empty() {
                load_req = load_req.mcp_servers(self.params.mcp_servers.clone());
            }
            let load_response = match self.protocol.load_session(load_req).await.map_err(AppError::from) {
                Ok(r) => r,
                Err(e) if is_session_not_found(&e) => {
                    return self.rebuild_after_session_not_found(session_id, &e).await;
                }
                Err(e) => return Err(e),
            };

            {
                let mut session = self.session.write().await;
                if let Some(models) = load_response.models {
                    session.apply_advertised_models(models);
                }
                if let Some(mut modes) = load_response.modes {
                    if let Some(db_current) = preloaded_mode {
                        modes.current_mode_id = db_current.into();
                    }
                    session.apply_advertised_modes(modes);
                }
                if let Some(config_options) = load_response.config_options {
                    session.apply_advertised_config_options(config_options);
                }
                session.set_session_id(DomainSessionId::new(session_id.to_owned()));
                session.mark_pending_session_new_prelude();
                session.mark_pending_knowledge_prelude();
                self.commit_session_changes(&mut session).await;
            }
            self.emit_snapshot_events().await;

            return match self.reconcile_session(session_id).await {
                Ok(()) => Ok(session_id.to_owned()),
                Err(e) if is_session_not_found(&e) => self.rebuild_after_session_not_found(session_id, &e).await,
                Err(e) => Err(e),
            };
        }

        // Legacy path: backend advertised neither claude-meta-resume nor
        // session/load. Seed the aggregate with the stored id and let the
        // caller prompt — matches pre-refactor behaviour.
        {
            let mut session = self.session.write().await;
            session.set_session_id(DomainSessionId::new(session_id.to_owned()));
            session.mark_pending_session_new_prelude();
            session.mark_pending_knowledge_prelude();
            self.commit_session_changes(&mut session).await;
        }
        self.emit_snapshot_events().await;
        match self.reconcile_session(session_id).await {
            Ok(()) => Ok(session_id.to_owned()),
            Err(e) if is_session_not_found(&e) => self.rebuild_after_session_not_found(session_id, &e).await,
            Err(e) => Err(e),
        }
    }

    /// Send a prompt to an already-established session.
    ///
    /// Returns `true` when the turn ended because it was CANCELLED (the
    /// `cancel()` path force-emitted the terminal `Finish(Cancelled)` for this
    /// turn already), `false` when this method emitted the terminal event
    /// (`Finish` for verified success, `Error` for empty/failed artifact
    /// delivery). Callers must NOT emit a further terminal event in either
    /// case — a late duplicate can land inside the NEXT turn's subscription
    /// (cancel-ack latency) and mis-terminate it.
    pub(super) async fn prompt_existing_session(
        &self,
        data: &SendMessageData,
        session_id: Option<&str>,
        turn: AgentRuntimeTurn,
    ) -> Result<bool, AppError> {
        let sid = session_id.ok_or_else(|| AppError::Internal("Cannot prompt: no session ID available".into()))?;

        let content = data.content.clone();

        // ACP notifications are partial and have no turn id. Reset the
        // session-scoped receipt immediately before Start/prompt so any
        // delivery failure observed by the notification handler belongs to
        // this turn and can veto a nominal EndTurn response.
        self.protocol.begin_artifact_delivery_turn(sid);

        // Emit Start event
        self.runtime.emit(AgentStreamEvent::Start(StartEventData {
            session_id: Some(sid.to_owned()),
        }));

        let prompt_response = self
            .protocol
            .prompt(PromptRequest::new(
                SessionId::new(sid),
                vec![ContentBlock::from(content)],
            ))
            .await
            .map_err(AppError::from)?;

        // ACP session/update notifications do not carry a turn id. Do not let
        // `cancel()` release turn admission before this prompt future reaches
        // its ordered PromptResponse boundary: otherwise late text/tool
        // updates from this prompt could be consumed by the next one. The
        // protocol response is therefore the authoritative cancel terminal.
        if matches!(prompt_response.stop_reason, StopReason::Cancelled) {
            // Close/reset the per-turn artifact receipt even on cancellation;
            // cancellation remains the user-visible terminal regardless of a
            // partial artifact that was abandoned with the turn.
            let _ = self.protocol.finish_artifact_delivery_turn(sid, false);
            emit_prompt_terminal(
                &self.runtime,
                turn,
                sid,
                prompt_response.stop_reason,
            );
            return Ok(true);
        }

        // A provider's PromptResponse only reports model control flow; it does
        // not attest that inline media/resources survived local persistence.
        // Delivery integrity therefore has precedence over EndTurn. Emit a
        // terminal Error and never follow it with Finish.
        let complete_verified_in_progress = matches!(prompt_response.stop_reason, StopReason::EndTurn);
        let completed_artifact_calls = match self
            .protocol
            .finish_artifact_delivery_turn(sid, complete_verified_in_progress)
        {
            Ok(completed) => completed,
            Err(error) => {
                emit_artifact_delivery_terminal(&self.runtime, turn, error);
                return Ok(false);
            }
        };
        for completed in completed_artifact_calls {
            if !self
                .runtime
                .emit_for_turn(turn, AgentStreamEvent::AcpToolCall(completed))
            {
                warn!(
                    session_id = sid,
                    "Discarding stale ACP artifact completion after its runtime turn closed"
                );
                return Ok(false);
            }
        }

        // Emit Finish event — carry the protocol stop_reason so AutoWork can
        // tell a clean EndTurn apart from a refusal / truncation
        // (otherwise a non-empty failed turn is silently recorded as done).
        // Guarded (absorbing-state) emit that also flips status → Finished:
        // exactly one terminal Finish per turn, emitted from exactly one place.
        emit_prompt_terminal(
            &self.runtime,
            turn,
            sid,
            prompt_response.stop_reason,
        );

        Ok(false)
    }

    /// Emit model/mode/config events from the session aggregate so the frontend
    /// receives the initial session state via WebSocket immediately after
    /// session creation or load.
    async fn emit_snapshot_events(&self) {
        use nomifun_api_types::{ModelInfoEntry, ModelInfoPayload};

        let session = self.session.read().await;
        if let Some(models) = session.model_info() {
            let current_id = models.current_model_id.to_string();
            let available: Vec<ModelInfoEntry> = models
                .available_models
                .iter()
                .map(|am| ModelInfoEntry {
                    id: am.model_id.to_string(),
                    label: am.name.clone(),
                })
                .collect();
            let current_label = available
                .iter()
                .find(|e| e.id == current_id)
                .map(|e| e.label.clone())
                .unwrap_or_else(|| current_id.clone());
            let payload = ModelInfoPayload {
                current_model_id: Some(current_id),
                current_model_label: Some(current_label),
                available_models: available,
            };
            // ModelInfoPayload is our own struct but go through the
            // normaliser for consistency with sibling events.
            if let Some(v) = sdk_to_snake_value(&payload) {
                self.runtime.emit(AgentStreamEvent::AcpModelInfo(v));
            }
        }
        if let Some(modes) = session.modes()
            && let Some(v) = sdk_to_snake_value(&modes)
        {
            self.runtime.emit(AgentStreamEvent::AcpModeInfo(v));
        }
        if let Some(config_options) = session.config_options()
            && let Some(v) = sdk_to_snake_value(&serde_json::json!({
                "config_options": config_options,
            }))
        {
            // Wrap in `{config_options: [...]}` to match the SDK
            // `ConfigOptionUpdate` shape used by the streaming path —
            // handshake blobs and downstream consumers see a uniform
            // structure regardless of origin.
            self.runtime.emit(AgentStreamEvent::AcpConfigOption(v));
        }
        if let Some(cmds) = session.available_commands() {
            self.runtime
                .emit(AgentStreamEvent::AvailableCommands(AvailableCommandsEventData {
                    commands: cmds.to_vec(),
                }));
        }
    }
}

fn artifact_delivery_terminal_error(detail: String) -> ErrorEventData {
    ErrorEventData::classified(
        "Nomifun could not verify the requested artifact delivery",
        AgentErrorCode::NomifunStateInconsistent,
        AgentErrorOwnership::Nomifun,
        Some(detail),
        true,
        true,
        Some(AgentErrorResolution::new(AgentErrorResolutionKind::Retry, None)),
    )
}

fn emit_artifact_delivery_terminal(runtime: &AgentRuntimeState, turn: AgentRuntimeTurn, detail: String) {
    runtime.emit_error_data_for_turn(turn, artifact_delivery_terminal_error(detail));
}

fn emit_prompt_terminal(
    runtime: &AgentRuntimeState,
    turn: AgentRuntimeTurn,
    session_id: &str,
    stop_reason: StopReason,
) -> bool {
    runtime.emit_finish_for_turn(
        turn,
        Some(session_id.to_owned()),
        Some(map_stop_reason(stop_reason)),
    )
}

/// Map the ACP SDK `StopReason` onto the cross-backend `TurnStopReason` carried
/// on the Finish event, so AutoWork can tell a clean completion apart from a
/// truncation / refusal / cancellation.
fn map_stop_reason(stop_reason: StopReason) -> TurnStopReason {
    match stop_reason {
        StopReason::EndTurn => TurnStopReason::EndTurn,
        StopReason::MaxTokens => TurnStopReason::MaxTokens,
        StopReason::MaxTurnRequests => TurnStopReason::MaxTurnRequests,
        StopReason::Refusal => TurnStopReason::Refusal,
        StopReason::Cancelled => TurnStopReason::Cancelled,
        // `StopReason` is #[non_exhaustive]: a future/unknown reason maps to
        // EndTurn (success) so we never falsely fail a real completion. If the
        // SDK adds a failure-class reason, add an explicit arm above.
        _ => TurnStopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    //! Contract tests for the post-`warmup_session` session invariant.
    //!
    //! The integration-test harness in `tests/acp_agent_integration.rs`
    //! cannot drive `AcpAgentManager` through a JSON-RPC mock today (all
    //! existing ACP tests there are `#[ignore]` for the same reason), so we
    //! pin the observable contract at the aggregate-root layer instead:
    //! whatever `warmup_session` does internally, the session aggregate
    //! must end up with `is_opened() == true` and a populated
    //! `session_id()` — the same terminal state the real `open_session_new`
    //! / `open_session_resume` helpers leave behind.
    use crate::manager::acp::{AcpSession, AcpSessionEvent};
    use crate::session::SessionId as DomainSessionId;
    use agent_client_protocol::schema::AgentCapabilities;
    use nomifun_api_types::{AgentErrorCode, AgentErrorOwnership};

    fn make_session() -> AcpSession {
        AcpSession::new(None, None, Default::default())
    }

    /// `open_session_resume` reads `session.agent_capabilities().load_session`
    /// to decide between `session/load` and the legacy seed-and-pray path.
    /// Reading from the SDK-typed advertised capabilities (instead of poking
    /// at the persisted handshake JSON) is the contract that ELECTRON-1HQ
    /// regressed against — OpenCode advertises `loadSession: true` on the
    /// wire, the SDK exposes it as `load_session: true`, but the old code
    /// looked up the snake-cased key in a JSON blob that hadn't always been
    /// written yet. Pin the contract: once the CLI has handshaken, the
    /// advertised slot must be populated and read back as the source of
    /// truth.
    #[test]
    fn advertised_capabilities_drives_supports_session_load() {
        let mut session = make_session();
        assert!(
            session.agent_capabilities().is_none(),
            "precondition: capabilities unset until init handshake completes"
        );

        // After `apply_advertised_capabilities` the resume path can answer
        // the question without consulting the persisted catalog row.
        let mut caps = AgentCapabilities::new();
        caps.load_session = true;
        session.apply_advertised_capabilities(caps);

        let supports_load = session.agent_capabilities().map(|c| c.load_session).unwrap_or(false);
        assert!(
            supports_load,
            "OpenCode-style `loadSession: true` handshake must enable session/load"
        );
    }

    #[test]
    fn missing_capability_means_no_session_load() {
        let session = make_session();
        let supports_load = session.agent_capabilities().map(|c| c.load_session).unwrap_or(false);
        assert!(
            !supports_load,
            "without an init handshake the resume path must not call session/load"
        );
    }

    #[test]
    fn capability_load_session_false_means_no_session_load() {
        let mut session = make_session();
        let caps = AgentCapabilities::new();
        // Default is load_session = false; assert reading it back agrees.
        session.apply_advertised_capabilities(caps);
        let supports_load = session.agent_capabilities().map(|c| c.load_session).unwrap_or(false);
        assert!(!supports_load);
    }

    /// Simulate the aggregate-state effect of a successful warmup that
    /// took the "open new session" path: `open_session_new` calls
    /// `set_session_id`, the outer `ensure_session_opened` then calls
    /// `mark_opened`. Post-state must satisfy both invariants so the
    /// follow-up `PUT /mode` / `PUT /model` can reconcile without
    /// re-opening.
    #[test]
    fn warmup_success_marks_session_opened_with_sid() {
        let mut session = make_session();
        assert!(!session.is_opened(), "precondition: session starts unopened");
        assert!(session.session_id().is_none(), "precondition: no sid yet");

        // open_session_new assigns the CLI-issued sid
        session.set_session_id(DomainSessionId::new("sess-warm-1"));
        // ensure_session_opened marks opened after the protocol call returns
        session.mark_opened();

        assert!(session.is_opened(), "warmup must leave session opened");
        assert_eq!(
            session.session_id(),
            Some("sess-warm-1"),
            "warmup must leave session id populated"
        );

        let events = session.drain_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AcpSessionEvent::SessionAssigned { .. })),
            "warmup must emit SessionAssigned for the persistence consumer"
        );
        assert!(
            events.iter().any(|e| matches!(e, AcpSessionEvent::SessionOpened)),
            "warmup must emit SessionOpened exactly once"
        );
    }

    /// When warmup encounters an already-opened session (e.g. called a
    /// second time on a warm agent), it must be a no-op — no duplicate
    /// `SessionOpened` event, sid preserved.
    #[test]
    fn warmup_on_opened_session_is_idempotent() {
        let mut session = make_session();
        session.set_session_id(DomainSessionId::new("sess-warm-2"));
        session.mark_opened();
        let _ = session.drain_events();

        // Second warmup call path: ensure_session_opened sees
        // (Some(sid), true) → no open_session_* call, but still flips
        // mark_opened (idempotent on the aggregate side).
        session.mark_opened();

        assert!(session.is_opened());
        assert_eq!(session.session_id(), Some("sess-warm-2"));
        assert!(
            session.drain_events().is_empty(),
            "second warmup must not emit duplicate domain events"
        );
    }

    /// `rebuild_after_session_not_found` relies on `clear_session_id`
    /// resetting both the sid and the `opened` flag, so the subsequent
    /// `ensure_session_opened` re-enters the `(None, _)` branch and
    /// calls `open_session_new`. Pin both invariants — without the
    /// `opened = false` reset, the rescue path would land in the
    /// `(Some, true)` no-op branch and the next prompt would still hit
    /// the dead session.
    #[test]
    fn clear_session_id_resets_sid_and_opened() {
        let mut session = make_session();
        session.set_session_id(DomainSessionId::new("ses-stale"));
        session.mark_opened();
        assert!(session.is_opened());
        assert_eq!(session.session_id(), Some("ses-stale"));

        session.clear_session_id();

        assert_eq!(session.session_id(), None, "stale sid must be dropped");
        assert!(
            !session.is_opened(),
            "rebuild requires re-running open_session_new — opened must reset"
        );
    }

    /// The `is_session_not_found` discriminator powers
    /// `open_session_resume`'s rescue path. Match strictly on the
    /// `AcpError::SessionNotFound -> AppError::NotFound` rendering;
    /// other 404s (e.g. workspace lookup) must surface to callers
    /// instead of triggering a phantom session rebuild.
    #[test]
    fn is_session_not_found_matches_session_not_found_only() {
        use nomifun_common::AppError;

        let session_err = AppError::NotFound("Session not found: ses-1".into());
        assert!(super::is_session_not_found(&session_err));

        let workspace_err = AppError::NotFound("Workspace not found".into());
        assert!(!super::is_session_not_found(&workspace_err));

        let bad_request = AppError::BadRequest("anything".into());
        assert!(!super::is_session_not_found(&bad_request));
    }

    use crate::protocol::events::AgentStreamEvent;
    use agent_client_protocol::schema::StopReason;

    /// `map_stop_reason` must carry each SDK stop reason through to the
    /// normalized `TurnStopReason` so AutoWork can classify the turn.
    #[test]
    fn map_stop_reason_maps_each_variant() {
        use crate::protocol::events::TurnStopReason;
        assert_eq!(super::map_stop_reason(StopReason::EndTurn), TurnStopReason::EndTurn);
        assert_eq!(super::map_stop_reason(StopReason::MaxTokens), TurnStopReason::MaxTokens);
        assert_eq!(
            super::map_stop_reason(StopReason::MaxTurnRequests),
            TurnStopReason::MaxTurnRequests
        );
        assert_eq!(super::map_stop_reason(StopReason::Refusal), TurnStopReason::Refusal);
        assert_eq!(super::map_stop_reason(StopReason::Cancelled), TurnStopReason::Cancelled);
    }

    #[tokio::test]
    async fn end_turn_without_observed_output_is_finish_not_synthetic_error() {
        let runtime = crate::runtime_state::AgentRuntimeState::new("conv-empty", "/tmp/workspace", 8);
        let turn = runtime.reset_for_new_turn(nomifun_common::ConversationStatus::Running);
        let mut rx = runtime.subscribe();

        assert!(super::emit_prompt_terminal(
            &runtime,
            turn,
            "session-empty",
            StopReason::EndTurn,
        ));

        let event = rx.recv().await.expect("prompt terminal event");
        let AgentStreamEvent::Finish(data) = event else {
            panic!("a nominal ACP EndTurn must never synthesize UNKNOWN_UPSTREAM_ERROR");
        };
        assert_eq!(data.session_id.as_deref(), Some("session-empty"));
        assert_eq!(data.stop_reason, Some(crate::protocol::events::TurnStopReason::EndTurn));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn artifact_delivery_failure_is_terminal_error_never_finish() {
        let runtime = crate::runtime_state::AgentRuntimeState::new("conv-1", "/tmp/workspace", 8);
        let turn = runtime.reset_for_new_turn(nomifun_common::ConversationStatus::Running);
        let mut rx = runtime.subscribe();

        super::emit_artifact_delivery_terminal(&runtime, turn, "invalid image bytes".into());

        let event = rx.recv().await.unwrap();
        let AgentStreamEvent::Error(error) = event else {
            panic!("artifact delivery failure must terminate with Error, not Finish");
        };
        assert_eq!(
            error.message,
            "Nomifun could not verify the requested artifact delivery"
        );
        assert_eq!(error.code, Some(AgentErrorCode::NomifunStateInconsistent));
        assert_eq!(error.ownership, Some(AgentErrorOwnership::Nomifun));
        assert_eq!(error.detail.as_deref(), Some("invalid image bytes"));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(runtime.status(), Some(nomifun_common::ConversationStatus::Finished));
    }
}

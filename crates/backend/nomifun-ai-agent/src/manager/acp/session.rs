use std::collections::HashMap;

use agent_client_protocol::schema::{
    AgentCapabilities, AuthMethod, AvailableCommand, ModelInfo, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOptions, SessionModeState, SessionModelState, UsageUpdate,
};

use super::agent_event_tracker::AcpSessionEvent;
use super::agent_reconcile::ReconcileAction;
use crate::protocol::error::CloseReason;
use crate::shared_kernel::{ConfigKey, ConfigValue, ModeId, ModelId, PersistedSessionState, SessionId};

/// What the user wants the session to be (intent).
#[derive(Debug, Clone, Default)]
struct Desired {
    mode_id: Option<ModeId>,
    model_id: Option<ModelId>,
    config_selections: HashMap<ConfigKey, ConfigValue>,
}

/// What the CLI last reported (ground truth from the backend).
#[derive(Debug, Clone, Default)]
struct Observed {
    mode_id: Option<ModeId>,
    model_id: Option<ModelId>,
    config_current: HashMap<ConfigKey, ConfigValue>,
}

/// What the CLI advertises as available options.
#[derive(Debug, Clone, Default)]
struct Advertised {
    modes: Option<SessionModeState>,
    models: Option<SessionModelState>,
    config_options: Option<Vec<SessionConfigOption>>,
    context_usage: Option<UsageUpdate>,
    agent_capabilities: Option<AgentCapabilities>,
    auth_methods: Option<Vec<AuthMethod>>,
    available_commands: Option<Vec<AvailableCommand>>,
}

/// Aggregate root for a single ACP session's lifecycle and state.
///
/// Encapsulates the three-layer state model (desired / observed / advertised)
/// and protects invariants:
/// - `session_id` is assigned at most once per lifecycle
/// - `desired.mode_id` must be in `advertised.modes` (when modes are known)
/// - `plan_reconcile` is a pure function: no side effects, fully testable
///
/// All mutations happen through aggregate methods which may emit domain
/// events (collected in `pending_events` and drained by the driver).
#[derive(Debug, Clone)]
pub struct AcpSession {
    session_id: Option<SessionId>,
    opened: bool,
    /// Whether `open_session_new` has just completed and the next prompt
    /// should receive preset_context / skill-index injection.
    ///
    /// Lifecycle:
    /// - writer: `AcpAgentManager::open_session_new` after a successful
    ///   `session/new` handshake.
    /// - reader: `SessionNewPreludeHook` via `take_pending_session_new_prelude`.
    /// - invalidation: any `take_*` call drains it to `false`.
    ///
    /// Starts `false` so resume paths, warmup-only flows, and aborted
    /// session/new attempts all correctly observe "no prelude pending".
    pending_session_new_prelude: bool,
    /// Whether the next prompt should carry the knowledge-base retrieval
    /// protocol section (`AcpSessionParams::knowledge_context`).
    ///
    /// Distinct from `pending_session_new_prelude`: the knowledge section is
    /// delivered on the first prompt of EVERY session activation — `session/new`
    /// AND every resume (`session/load`, claude-meta-resume, legacy seed) — so a
    /// resumed or restarted session, or one rebuilt after a `挂载知识库` change,
    /// still receives the protocol. The new-session prelude (preset rules +
    /// skill index) deliberately stays new-session-only; this flag does not.
    ///
    /// Lifecycle:
    /// - writer: `open_session_new` and every successful `open_session_resume`
    ///   branch via `mark_pending_knowledge_prelude`.
    /// - reader: `KnowledgeContextHook` via `take_pending_knowledge_prelude`.
    /// - invalidation: drained to `false` on the first prompt after open.
    pending_knowledge_prelude: bool,
    desired: Desired,
    observed: Observed,
    advertised: Advertised,
    pending_events: Vec<AcpSessionEvent>,
    /// Model id the next prompt should announce to the CLI via an
    /// injected `<system-reminder>`. Written when the CLI bakes
    /// model identity into its cached system prompt (see
    /// `BehaviorPolicy::self_identity_sticky`) and `session/set_model`
    /// therefore does not refresh the LLM's self-description.
    /// Taken (drained) on the next prompt.
    pending_model_notice: Option<ModelId>,
    /// Why the session most recently terminated, if at all.
    ///
    /// Lifecycle (see also `CloseReason` doc comment):
    /// - writer: `record_close_reason`, called by the manager from each
    ///   close path (`send_message` Err, `cancel`, `kill`, post-init exit
    ///   detection).
    /// - reader: `last_close_reason` (non-destructive) for diagnostics and
    ///   `take_close_reason` (drain) by the toast-builder right before the
    ///   `Error` event broadcast.
    /// - invalidation: cleared on `clear_session_id` so a rebuilt session
    ///   does not inherit the previous turn's close reason.
    last_close_reason: Option<CloseReason>,
}

impl AcpSession {
    pub fn new(
        initial_mode: Option<ModeId>,
        initial_model: Option<ModelId>,
        config_selections: HashMap<ConfigKey, ConfigValue>,
    ) -> Self {
        Self {
            session_id: None,
            opened: false,
            pending_session_new_prelude: false,
            pending_knowledge_prelude: false,
            desired: Desired {
                mode_id: initial_mode,
                model_id: initial_model,
                config_selections,
            },
            observed: Observed::default(),
            advertised: Advertised::default(),
            pending_events: Vec::new(),
            pending_model_notice: None,
            last_close_reason: None,
        }
    }
}

// ─── Session Id and Session Opened ───────────────────────────────────────────────────────
impl AcpSession {
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_ref().map(SessionId::as_str)
    }

    pub fn session_id_vo(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    /// Assign (or restore) a session ID. Idempotent: re-assigning the same
    /// ID is a no-op. Assigning a *different* ID after one is already set
    /// is an invariant violation (the aggregate must be recreated).
    pub fn set_session_id(&mut self, sid: SessionId) {
        if let Some(existing) = &self.session_id {
            debug_assert_eq!(existing, &sid, "session_id reassignment attempted");
            return;
        }
        self.session_id = Some(sid.clone());
        self.pending_events
            .push(AcpSessionEvent::SessionAssigned { session_id: sid });
    }

    /// Drop a stale session id so the aggregate can be re-seeded with a
    /// freshly-issued one. Used when the CLI rejects the persisted sid
    /// with `SessionNotFound` (ELECTRON-1HQ): the resume helpers fall
    /// back to `open_session_new`, which calls `set_session_id` again.
    /// Also clears the `opened` flag so the next `ensure_session_opened`
    /// goes down the "no sid" branch instead of the "sid+opened" no-op.
    pub fn clear_session_id(&mut self) {
        self.session_id = None;
        self.opened = false;
        // A rebuilt session must not inherit the prior turn's close reason —
        // otherwise the next user-facing error would surface stale context.
        self.last_close_reason = None;
    }

    /// Record the reason the most recent turn closed. Overwrites any
    /// previous reason — only the latest one is meaningful for the next
    /// user-facing toast. Pass `None` to clear (rare; mostly used by tests
    /// and `clear_session_id`).
    pub fn record_close_reason(&mut self, reason: Option<CloseReason>) {
        self.last_close_reason = reason;
    }

    /// Read the last close reason without consuming it. Used for
    /// diagnostics and for tests.
    pub fn last_close_reason(&self) -> Option<&CloseReason> {
        self.last_close_reason.as_ref()
    }

    /// Drain the last close reason. Called by the close-path handler in
    /// `AcpAgentManager` right before broadcasting the `Error` event so
    /// the same reason is not re-rendered on a follow-up request.
    pub fn take_close_reason(&mut self) -> Option<CloseReason> {
        self.last_close_reason.take()
    }

    pub fn is_opened(&self) -> bool {
        self.opened
    }

    /// Mark the session as opened with the CLI (first turn handshake complete).
    pub fn mark_opened(&mut self) {
        if !self.opened {
            self.opened = true;
            self.pending_events.push(AcpSessionEvent::SessionOpened);
        }
    }

    /// Set the flag signalling that the next prompt carries the first
    /// post-`session/new` payload. Idempotent.
    pub fn mark_pending_session_new_prelude(&mut self) {
        self.pending_session_new_prelude = true;
    }

    /// Consume the prelude flag. Returns `true` exactly once after
    /// `mark_pending_session_new_prelude`; subsequent calls return `false`.
    pub fn take_pending_session_new_prelude(&mut self) -> bool {
        std::mem::replace(&mut self.pending_session_new_prelude, false)
    }

    /// Set the flag signalling that the next prompt should carry the knowledge
    /// retrieval-protocol section. Called on every session activation (new and
    /// resume). Idempotent.
    pub fn mark_pending_knowledge_prelude(&mut self) {
        self.pending_knowledge_prelude = true;
    }

    /// Consume the knowledge-prelude flag. Returns `true` exactly once per
    /// session activation; subsequent calls in the same session return `false`.
    pub fn take_pending_knowledge_prelude(&mut self) -> bool {
        std::mem::replace(&mut self.pending_knowledge_prelude, false)
    }
}

// ─── Getters Setters desired ───────────────────────────────────────────────────────
impl AcpSession {
    pub fn desired_mode(&self) -> Option<&str> {
        self.desired.mode_id.as_ref().map(ModeId::as_str)
    }

    pub fn desired_mode_id(&self) -> Option<&ModeId> {
        self.desired.mode_id.as_ref()
    }

    pub fn desired_model(&self) -> Option<&str> {
        self.desired.model_id.as_ref().map(ModelId::as_str)
    }

    pub fn desired_model_id(&self) -> Option<&ModelId> {
        self.desired.model_id.as_ref()
    }

    pub fn desired_config_selections(&self) -> &HashMap<ConfigKey, ConfigValue> {
        &self.desired.config_selections
    }

    /// Whether the requested model can be selected in the current session.
    ///
    /// Before the ACP backend advertises models, keep the historical permissive
    /// behavior so initial seeds can still be reconciled once the session opens.
    pub fn can_select_model(&self, model_id: &str) -> bool {
        !model_id.is_empty() && self.is_model_valid(model_id)
    }

    /// Set the user's desired mode. Emits `DesiredModeChanged` if the
    /// value actually changed. When advertised modes are known, the mode
    /// must be in the list (otherwise the call is a no-op).
    pub fn set_desired_mode(&mut self, mode: ModeId) -> bool {
        if mode.as_str().is_empty() {
            return false;
        }
        if !self.is_mode_valid(mode.as_str()) {
            return false;
        }
        if self.desired.mode_id.as_ref() == Some(&mode) {
            return false;
        }
        self.desired.mode_id = Some(mode.clone());
        self.pending_events.push(AcpSessionEvent::DesiredModeChanged { mode });
        true
    }

    /// Set the user's desired model. Emits `DesiredModelChanged` if the
    /// value actually changed. When advertised models are known, the model
    /// must be in the list (otherwise the call is a no-op).
    pub fn set_desired_model(&mut self, model: ModelId) -> bool {
        if model.as_str().is_empty() {
            return false;
        }
        if !self.is_model_valid(model.as_str()) {
            return false;
        }
        if self.desired.model_id.as_ref() == Some(&model) {
            return false;
        }
        self.desired.model_id = Some(model.clone());
        self.pending_events.push(AcpSessionEvent::DesiredModelChanged { model });
        true
    }

    /// Drop a desired model that is not advertised by the active ACP session.
    ///
    /// Initial model seeds can be loaded before `session/new` reports the
    /// provider's available models. Once advertised models are known, reconcile
    /// must not issue `session/set_model` for a stale seed.
    pub fn clear_invalid_desired_model(&mut self) -> Option<ModelId> {
        let model = self.desired.model_id.clone()?;
        if self.is_model_valid(model.as_str()) {
            return None;
        }
        self.desired.model_id = None;
        if self.pending_model_notice.as_ref() == Some(&model) {
            self.pending_model_notice = None;
        }
        Some(model)
    }

    /// Set a user's desired config selection.
    pub fn set_desired_config(&mut self, key: ConfigKey, value: ConfigValue) {
        let changed = self.desired.config_selections.get(&key) != Some(&value);
        self.desired.config_selections.insert(key, value);
        if changed {
            let selections = self.desired.config_selections.clone();
            self.pending_events
                .push(AcpSessionEvent::DesiredConfigChanged { selections });
        }
    }
}

// ─── Getters observed ───────────────────────────────────────────────────────
impl AcpSession {
    pub fn observed_mode(&self) -> Option<&str> {
        self.observed.mode_id.as_ref().map(ModeId::as_str)
    }

    pub fn observed_mode_id(&self) -> Option<&ModeId> {
        self.observed.mode_id.as_ref()
    }

    pub fn observed_model(&self) -> Option<&str> {
        self.observed.model_id.as_ref().map(ModelId::as_str)
    }

    pub fn observed_model_id(&self) -> Option<&ModelId> {
        self.observed.model_id.as_ref()
    }
}

// ─── Getters advertised ───────────────────────────────────────────────────────
impl AcpSession {
    pub fn modes(&self) -> Option<&SessionModeState> {
        self.advertised.modes.as_ref()
    }

    pub fn model_info(&self) -> Option<&SessionModelState> {
        self.advertised.models.as_ref()
    }

    pub fn config_options(&self) -> Option<&[SessionConfigOption]> {
        self.advertised.config_options.as_deref()
    }

    pub fn context_usage(&self) -> Option<&UsageUpdate> {
        self.advertised.context_usage.as_ref()
    }

    pub fn agent_capabilities(&self) -> Option<&AgentCapabilities> {
        self.advertised.agent_capabilities.as_ref()
    }

    pub fn auth_methods(&self) -> Option<&[AuthMethod]> {
        self.advertised.auth_methods.as_deref()
    }

    pub fn available_commands(&self) -> Option<&[AvailableCommand]> {
        self.advertised.available_commands.as_deref()
    }

    pub fn current_mode_id(&self) -> Option<String> {
        self.advertised.modes.as_ref().map(|m| m.current_mode_id.to_string())
    }

    pub fn current_model_id(&self) -> Option<String> {
        self.advertised.models.as_ref().map(|m| m.current_model_id.to_string())
    }
}

// ─── Observations (from CLI responses/notifications) ───────────────
impl AcpSession {
    /// Record the CLI's current mode. Updates both `observed.mode_id` and
    /// the `advertised.modes.current_mode_id` (available_modes preserved);
    /// emits `ObservedModeSynced` when the value actually changed.
    pub fn apply_observed_mode(&mut self, mode: ModeId) {
        let changed = self.observed.mode_id.as_ref() != Some(&mode);
        self.observed.mode_id = Some(mode.clone());
        let available = self
            .advertised
            .modes
            .as_ref()
            .map(|m| m.available_modes.clone())
            .unwrap_or_default();
        self.advertised.modes = Some(SessionModeState::new(mode.as_str().to_owned(), available));
        if changed {
            self.pending_events.push(AcpSessionEvent::ObservedModeSynced { mode });
        }
    }

    /// Record the CLI's current model. Updates both `observed.model_id` and
    /// the `advertised.models.current_model_id` (available_models preserved);
    /// emits `ObservedModelSynced` when the value actually changed.
    pub fn apply_observed_model(&mut self, model: ModelId) {
        let changed = self.observed.model_id.as_ref() != Some(&model);
        self.observed.model_id = Some(model.clone());
        let available = self
            .advertised
            .models
            .as_ref()
            .map(|m| m.available_models.clone())
            .unwrap_or_default();
        self.advertised.models = Some(SessionModelState::new(model.as_str().to_owned(), available));
        if changed {
            self.pending_events.push(AcpSessionEvent::ObservedModelSynced { model });
        }
    }

    /// Record the CLI's current value for a single config option. Mirrors
    /// `apply_observed_mode/model`: diff-driven, emits `ObservedConfigSynced`
    /// with the full selection map when the value actually changed. Used by
    /// the reconcile loop after a successful `set_config_option` so
    /// `plan_reconcile` treats the drift as resolved.
    pub fn apply_observed_config(&mut self, key: ConfigKey, value: ConfigValue) {
        let changed = self.observed.config_current.get(&key) != Some(&value);
        self.observed.config_current.insert(key, value);
        if changed {
            let selections = self.observed.config_current.clone();
            self.pending_events
                .push(AcpSessionEvent::ObservedConfigSynced { selections });
        }
    }

    pub fn apply_advertised_modes(&mut self, modes: SessionModeState) {
        let new_id = ModeId::new(modes.current_mode_id.to_string());
        let changed = self.observed.mode_id.as_ref() != Some(&new_id);
        self.observed.mode_id = Some(new_id.clone());
        self.advertised.modes = Some(modes);
        if changed {
            self.pending_events
                .push(AcpSessionEvent::ObservedModeSynced { mode: new_id });
        }
    }

    pub fn apply_advertised_models(&mut self, models: SessionModelState) {
        let new_id = ModelId::new(models.current_model_id.to_string());
        let changed = self.observed.model_id.as_ref() != Some(&new_id);
        self.observed.model_id = Some(new_id.clone());
        self.advertised.models = Some(models);
        if changed {
            self.pending_events
                .push(AcpSessionEvent::ObservedModelSynced { model: new_id });
        }
    }

    pub fn apply_advertised_config_options(&mut self, options: Vec<SessionConfigOption>) {
        let mut changed = false;
        for opt in &options {
            if let Some(current) = extract_config_current_value(&opt.kind) {
                let key = ConfigKey::new(opt.id.to_string());
                let value = ConfigValue::new(current);
                if self.observed.config_current.insert(key, value.clone()).as_ref() != Some(&value) {
                    changed = true;
                }
            }
        }
        // Newer ACP agents such as OpenCode expose model selection through
        // the stable `configOptions` API instead of the older top-level
        // unstable `models` field. Project that standard model selector into
        // SessionModelState so existing model APIs and UI work with both
        // protocol representations.
        if let Some(models) = model_state_from_config_options(&options) {
            self.apply_advertised_models(models);
        }
        self.advertised.config_options = Some(options);
        if changed {
            let selections = self.observed.config_current.clone();
            self.pending_events
                .push(AcpSessionEvent::ObservedConfigSynced { selections });
        }
    }

    pub fn apply_advertised_capabilities(&mut self, caps: AgentCapabilities) {
        self.advertised.agent_capabilities = Some(caps);
    }

    pub fn apply_advertised_auth_methods(&mut self, methods: Vec<AuthMethod>) {
        self.advertised.auth_methods = Some(methods);
    }

    pub fn apply_advertised_commands(&mut self, commands: Vec<AvailableCommand>) {
        self.advertised.available_commands = Some(commands);
    }

    /// Record the CLI's latest context usage. Diff-driven: emits
    /// `ObservedContextUsageChanged` only when the usage payload differs
    /// from what we last cached, so the persistence consumer can debounce
    /// a stream of token updates into one DB write per turn.
    pub fn apply_context_usage(&mut self, usage: UsageUpdate) {
        let changed = self.advertised.context_usage.as_ref() != Some(&usage);
        self.advertised.context_usage = Some(usage.clone());
        if changed {
            let usage_json = serde_json::to_string(&usage).unwrap_or_default();
            self.pending_events
                .push(AcpSessionEvent::ObservedContextUsageChanged { usage_json });
        }
    }
}

impl AcpSession {
    /// Seed the aggregate with persisted user choices from DB.
    /// Called on resume paths before the CLI session/load response arrives.
    pub fn preload_persisted(&mut self, state: &PersistedSessionState) {
        if let Some(mode) = &state.current_mode_id {
            self.advertised.modes = Some(SessionModeState::new(mode.as_str().to_owned(), Vec::new()));
            self.observed.mode_id = Some(mode.clone());
        }
        if let Some(model) = &state.current_model_id {
            self.advertised.models = Some(SessionModelState::new(model.as_str().to_owned(), Vec::new()));
            self.observed.model_id = Some(model.clone());
        }
        if !state.config_selections.is_empty() {
            self.observed.config_current = state.config_selections.clone();
        }
        if let Some(usage) = &state.context_usage {
            self.advertised.context_usage = Some(usage.clone());
        }
    }
}

// ─── Reconcile ─────────────────────────────────────────────────────
impl AcpSession {
    /// Produce a list of actions needed to align CLI state with user intent.
    /// Pure function — no side effects. The driver executes the actions.
    pub fn plan_reconcile(&self) -> Vec<ReconcileAction> {
        let mut actions = Vec::new();

        if let Some(desired_mode) = &self.desired.mode_id
            && self.observed.mode_id.as_ref() != Some(desired_mode)
        {
            actions.push(ReconcileAction::SetMode {
                mode: desired_mode.clone(),
            });
        }

        if let Some(desired_model) = &self.desired.model_id
            && self.observed.model_id.as_ref() != Some(desired_model)
        {
            actions.push(ReconcileAction::SetModel {
                model: desired_model.clone(),
            });
        }

        for (key, desired_value) in &self.desired.config_selections {
            if self.observed.config_current.get(key) != Some(desired_value) {
                actions.push(ReconcileAction::SetConfigOption {
                    key: key.clone(),
                    value: desired_value.clone(),
                });
            }
        }

        actions
    }

    // ─── Event drain ───────────────────────────────────────────────────

    /// Consume and return all pending domain events.
    pub fn drain_events(&mut self) -> Vec<AcpSessionEvent> {
        std::mem::take(&mut self.pending_events)
    }

    /// Record the model id that the next prompt should announce to the
    /// CLI via a `<system-reminder>`. See `pending_model_notice` for the
    /// motivating invariant.
    pub fn set_pending_model_notice(&mut self, model: ModelId) {
        self.pending_model_notice = Some(model);
    }

    /// Drain the pending model notice (if any). Callers consume the
    /// value before sending the next prompt so it is not re-injected.
    pub fn take_pending_model_notice(&mut self) -> Option<ModelId> {
        self.pending_model_notice.take()
    }

    // ─── Private helpers ───────────────────────────────────────────────

    fn is_mode_valid(&self, mode_id: &str) -> bool {
        match &self.advertised.modes {
            None => true,
            Some(modes) if modes.available_modes.is_empty() => true,
            Some(modes) => modes.available_modes.iter().any(|m| m.id.0.as_ref() == mode_id),
        }
    }

    fn is_model_valid(&self, model_id: &str) -> bool {
        match &self.advertised.models {
            None => true,
            Some(models) if models.available_models.is_empty() => true,
            Some(models) => models
                .available_models
                .iter()
                .any(|m| m.model_id.0.as_ref() == model_id),
        }
    }
}

fn extract_config_current_value(kind: &SessionConfigKind) -> Option<String> {
    match kind {
        SessionConfigKind::Select(sel) => Some(sel.current_value.to_string()),
        _ => None,
    }
}

fn model_state_from_config_options(options: &[SessionConfigOption]) -> Option<SessionModelState> {
    let option = options.iter().find(|option| {
        matches!(option.category, Some(SessionConfigOptionCategory::Model))
            || option.id.to_string().eq_ignore_ascii_case("model")
    })?;
    let SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };

    let available_models = match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .map(|option| ModelInfo::new(option.value.to_string(), option.name.clone()))
            .collect(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| {
                group
                    .options
                    .iter()
                    .map(|option| ModelInfo::new(option.value.to_string(), option.name.clone()))
            })
            .collect(),
        _ => return None,
    };

    Some(SessionModelState::new(
        select.current_value.to_string(),
        available_models,
    ))
}

// Tests live in `session_tests.rs` (linked via `#[path]`) so this file
// stays under the 1000-line per-file budget. Inside that file `super::*`
// resolves to this module's private items.
#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;

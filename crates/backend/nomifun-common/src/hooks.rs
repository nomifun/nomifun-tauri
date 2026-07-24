//! Cross-crate lifecycle hook traits.
//!
//! Hooks defined here let lower-layer crates (e.g. `nomifun-ai-agent`,
//! `nomifun-cron`) react to events owned by higher-layer crates (e.g.
//! `nomifun-conversation`) without forming a dependency cycle.

use async_trait::async_trait;

/// Notified when a conversation row is deleted via
/// `ConversationService::delete`.
///
/// Implementors are responsible for cleaning up their per-conversation state
/// (kill agent processes, drop cron jobs, etc.). Hooks run sequentially in
/// registration order; failures must be logged inside the hook and not
/// propagated. `user_id` is the verified conversation owner captured before
/// deletion, so cleanup code can emit owner-scoped lifecycle events even after
/// the conversation row no longer exists.
#[async_trait]
pub trait OnConversationDelete: Send + Sync {
    async fn on_conversation_deleted(&self, user_id: &str, conversation_id: &str);
}

/// Notified when a terminal session row is deleted via
/// `TerminalService::delete`.
///
/// Mirrors [`OnConversationDelete`] for the terminal domain. Lets lower-layer
/// crates react to a terminal going away without `nomifun-terminal` depending
/// on them (e.g. `nomifun-requirement` clears the dual-domain
/// `owner_session_id`/`owner_kind` of requirements owned by a terminal UUIDv7,
/// which has no physical FK to cascade — spec §9.B).
///
/// Implementors are responsible for cleaning up their per-terminal state. Hooks
/// run sequentially in registration order; failures must be logged inside the
/// hook and not propagated. `user_id` is the verified terminal owner captured
/// before deletion, so polymorphic cleanup remains owner-scoped after the row
/// itself is gone.
#[async_trait]
pub trait OnTerminalDelete: Send + Sync {
    async fn on_terminal_deleted(&self, user_id: &str, terminal_id: &str);
}

/// Creates a tracked requirement from an inbound channel message (the opt-in
/// IM → requirement pipeline). Lets `nomifun-channel` file a message as a
/// requirement without depending on `nomifun-requirement`; the concrete
/// implementor (in `nomifun-requirement`) delegates to `RequirementService`.
/// Creating a `Pending` requirement is enough — AutoWork is woken to execute it.
#[async_trait]
pub trait RequirementCreator: Send + Sync {
    /// Create a Pending requirement. `tag` is the board column to file under
    /// (e.g. "inbox"); `created_by` records the origin (e.g. "channel:slack").
    /// Returns the new requirement's stable bare UUIDv7 on success.
    async fn create_from_message(
        &self,
        title: &str,
        content: &str,
        tag: &str,
        created_by: &str,
    ) -> Result<String, String>;
}

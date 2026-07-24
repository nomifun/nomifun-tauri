use std::sync::Arc;

use crate::service::TerminalService;

/// Best-effort bridge from user-initiated terminal lifecycle actions back to
/// the live owner conversation. The terminal crate stays independent from any
/// concrete agent runtime; the application supplies the bridge.
pub trait TerminalConversationNoticeSink: Send + Sync {
    fn notify_terminal_lifecycle(
        &self,
        conversation_id: &str,
        terminal_id: &str,
        event: &'static str,
    );
}

/// Router state for the terminal module.
#[derive(Clone)]
pub struct TerminalRouterState {
    pub terminal_service: Arc<TerminalService>,
    pub conversation_notice_sink: Option<Arc<dyn TerminalConversationNoticeSink>>,
}

impl TerminalRouterState {
    pub fn new(terminal_service: Arc<TerminalService>) -> Self {
        Self {
            terminal_service,
            conversation_notice_sink: None,
        }
    }

    pub fn with_conversation_notice_sink(
        mut self,
        sink: Arc<dyn TerminalConversationNoticeSink>,
    ) -> Self {
        self.conversation_notice_sink = Some(sink);
        self
    }

    pub fn notify_owner(
        &self,
        conversation_id: &str,
        terminal_id: &str,
        event: &'static str,
    ) {
        if let Some(sink) = &self.conversation_notice_sink {
            sink.notify_terminal_lifecycle(conversation_id, terminal_id, event);
        }
    }
}

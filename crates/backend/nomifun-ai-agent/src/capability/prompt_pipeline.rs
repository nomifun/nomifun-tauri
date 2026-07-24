//! Thin hook-chain dispatcher for outgoing ACP prompts.
//!
//! Registration order equals execution order; each hook's output feeds the
//! next hook's input.

use crate::capability::skill_manager::AcpSkillManager;
use crate::factory::acp_assembler::AcpSessionParams;
use crate::manager::acp::AcpSession;
use std::sync::Arc;

/// Read/write slice handed to each hook. `session` is a mutable borrow
/// so hooks can consume one-shot flags (e.g. `take_pending_model_notice`).
pub struct PromptCtx<'a> {
    pub session: &'a mut AcpSession,
    pub params: &'a AcpSessionParams,
    pub skill_manager: &'a Arc<AcpSkillManager>,
}

#[async_trait::async_trait]
pub trait PreSendHook: Send + Sync {
    async fn pre_send(&self, ctx: &mut PromptCtx<'_>, prompt: String) -> String;
}

pub struct PromptPipeline {
    hooks: Vec<Arc<dyn PreSendHook>>,
}

impl PromptPipeline {
    pub fn new(hooks: Vec<Arc<dyn PreSendHook>>) -> Self {
        Self { hooks }
    }

    pub async fn pre_send(&self, ctx: &mut PromptCtx<'_>, prompt: String) -> String {
        let mut current = prompt;
        for hook in &self.hooks {
            current = hook.pre_send(ctx, current).await;
        }
        current
    }
}

//! Built-in `PreSendHook`s for the ACP prompt pipeline.
//!
//! Each hook reads a one-shot flag on `AcpSession` (or a `pending_*`
//! field), consumes it, and prepends its block to the prompt.

use crate::capability::first_message_injector::{InjectionConfig, inject_first_message_prefix};
use crate::capability::model_identity_reminder::render_model_identity_reminder;
use crate::capability::prompt_pipeline::{PreSendHook, PromptCtx};

#[derive(Default)]
pub struct SessionNewPreludeHook;

#[async_trait::async_trait]
impl PreSendHook for SessionNewPreludeHook {
    async fn pre_send(&self, ctx: &mut PromptCtx<'_>, prompt: String) -> String {
        if !ctx.session.take_pending_session_new_prelude() {
            return prompt;
        }

        let metadata = &ctx.params.metadata;
        let config = InjectionConfig {
            preset_context: ctx.params.preset_context.as_deref(),
            skills: &ctx.params.config.skills,
            custom_workspace: ctx.params.workspace.is_custom,
            native_skill_support: metadata
                .native_skills_dirs
                .as_ref()
                .is_some_and(|v: &Vec<String>| !v.is_empty()),
        };

        // Skill discovery is best-effort inside the injector; when no usable
        // prelude can be built, the original prompt continues unchanged.
        inject_first_message_prefix(&prompt, ctx.skill_manager, config).await
    }
}

/// Deliver the knowledge-base retrieval-protocol section
/// (`AcpSessionParams::knowledge_context`) on the first prompt of EVERY session
/// activation — `session/new` and every resume path. Unlike
/// `SessionNewPreludeHook` (preset rules + skill index, new-session-only), this
/// hook fires on resume too, so a resumed/restarted session — or one rebuilt
/// after a `挂载知识库` binding change — still learns which bases are mounted and
/// how to retrieve from them. Consumes the one-shot `pending_knowledge_prelude`
/// flag set by `open_session_new` / `open_session_resume`.
#[derive(Default)]
pub struct KnowledgeContextHook;

#[async_trait::async_trait]
impl PreSendHook for KnowledgeContextHook {
    async fn pre_send(&self, ctx: &mut PromptCtx<'_>, prompt: String) -> String {
        if !ctx.session.take_pending_knowledge_prelude() {
            return prompt;
        }
        match ctx.params.knowledge_context.as_deref() {
            Some(section) if !section.is_empty() => {
                format!("[Knowledge Bases]\n{section}\n[/Knowledge Bases]\n\n{prompt}")
            }
            _ => prompt,
        }
    }
}

#[derive(Default)]
pub struct ModelIdentityReminderHook;

#[async_trait::async_trait]
impl PreSendHook for ModelIdentityReminderHook {
    async fn pre_send(&self, ctx: &mut PromptCtx<'_>, prompt: String) -> String {
        let Some(model) = ctx.session.take_pending_model_notice() else {
            return prompt;
        };

        // Prefer the advertised human-readable label over the raw id.
        let label = ctx
            .session
            .model_info()
            .and_then(|m| {
                m.available_models
                    .iter()
                    .find(|am| am.model_id.0.as_ref() == model.as_str())
                    .map(|am| am.name.clone())
            })
            .unwrap_or_else(|| model.as_str().to_owned());

        let reminder = render_model_identity_reminder(&label);
        format!("{reminder}{prompt}")
    }
}

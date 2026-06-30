//! Production [`TerminalTitleCompleter`]: summarize a terminal session's first
//! turn into a short work-content title using the default provider/model.
//!
//! Same layering as [`LiveKnowledgeCompleter`](crate::knowledge_completer): the
//! trait lives in `nomifun-terminal` (the lower crate), this crate provides the
//! provider-backed implementation, and `nomifun-app` wires it via
//! `TerminalService::with_title_completer`. There is no per-feature model
//! setting — auto-titling is a cheap background touch, so the default is the
//! first enabled provider (registry order) and its first enabled model.

use std::path::PathBuf;
use std::sync::Arc;

use nomifun_common::AppError;
use nomifun_db::IProviderRepository;
use nomifun_terminal::TerminalTitleCompleter;

use crate::factory::provider_config::{one_shot_completion, resolve_provider_config, user_message};
use crate::knowledge_completer::first_enabled_model;

/// The reply is a single short line; a tiny budget keeps the call cheap and
/// prevents a runaway model from emitting a paragraph instead of a title.
const TITLE_MAX_TOKENS: u32 = 64;

/// System prompt: produce ONE short work-content title — no quotes, no trailing
/// punctuation, no explanation — in the same language as the input.
const TITLE_SYSTEM: &str = "\
You name a terminal session by its work content. Read the snippet (a user's first \
command or prompt, and/or the assistant's first reply) and output ONE short title \
describing the work being done. Rules: at most 6 words or about 16 characters; no \
surrounding quotes; no trailing punctuation; no preamble or explanation; reply in \
the SAME language as the input (Chinese input → Chinese title). Output only the title.";

/// Provider-backed terminal title generator.
pub struct LiveTerminalTitleCompleter {
    pub provider_repo: Arc<dyn IProviderRepository>,
    pub encryption_key: [u8; 32],
    pub workspace: PathBuf,
}

impl LiveTerminalTitleCompleter {
    /// First enabled provider (creation order) + its first enabled model.
    async fn resolve_default_model(&self) -> Result<(String, String), AppError> {
        let providers = self
            .provider_repo
            .list()
            .await
            .map_err(|e| AppError::Internal(format!("failed to list providers: {e}")))?;
        for provider in providers.iter().filter(|p| p.enabled) {
            if let Some(model) = first_enabled_model(&provider.models, provider.model_enabled.as_deref()) {
                return Ok((provider.id.clone(), model));
            }
        }
        Err(AppError::Conflict(
            "terminal auto-title unavailable: no enabled provider/model is configured".into(),
        ))
    }
}

#[async_trait::async_trait]
impl TerminalTitleCompleter for LiveTerminalTitleCompleter {
    async fn summarize(&self, content: &str) -> Result<String, AppError> {
        let (provider_id, model) = self.resolve_default_model().await?;
        let cfg = resolve_provider_config(
            &self.provider_repo,
            &self.encryption_key,
            &provider_id,
            &model,
            &self.workspace,
        )
        .await?;
        one_shot_completion(&cfg, TITLE_SYSTEM, vec![user_message(content)], TITLE_MAX_TOKENS).await
    }
}

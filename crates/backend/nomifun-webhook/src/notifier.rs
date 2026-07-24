//! Completion notifier: implements `nomifun_requirement::CompletionNotifier` by
//! looking up the requirement's tag → bound webhook and sending a notification.
//!
//! Dependency direction: this crate depends on `nomifun-requirement` (for the
//! trait); `nomifun-requirement` does NOT depend on this crate. Mirrors how
//! `nomifun-idmm` implements `nomifun_requirement::IdmmHandle`.

use std::sync::Arc;

use async_trait::async_trait;
use nomifun_api_types::WebhookPlatform;
use nomifun_db::models::RequirementRow;
use nomifun_db::{ITagSettingRepository, IWebhookRepository};
use nomifun_requirement::CompletionNotifier;

use crate::sender::WebhookSender;

/// Truncate a content snippet for the notification card (keeps cards compact).
const MAX_CONTENT_CHARS: usize = 500;

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}…")
}

/// Human-readable completion status for the 【完成状态】 field.
fn status_label(status: &str) -> &'static str {
    match status {
        "done" => "已完成 (done)",
        "failed" => "失败 (failed)",
        "cancelled" => "已取消 (cancelled)",
        _ => "完成 (completed)",
    }
}

/// Whether `status` is in the per-tag allowed event set.
pub fn event_allowed(status: &str, events: &[String]) -> bool {
    events.iter().any(|e| e == status)
}

pub struct CompletionNotifierImpl {
    tag_settings: Arc<dyn ITagSettingRepository>,
    webhooks: Arc<dyn IWebhookRepository>,
    sender: Arc<dyn WebhookSender>,
}

impl CompletionNotifierImpl {
    pub fn new(
        tag_settings: Arc<dyn ITagSettingRepository>,
        webhooks: Arc<dyn IWebhookRepository>,
        sender: Arc<dyn WebhookSender>,
    ) -> Self {
        Self {
            tag_settings,
            webhooks,
            sender,
        }
    }

    pub fn into_arc(self) -> Arc<dyn CompletionNotifier> {
        Arc::new(self)
    }

    /// Resolve the bound + enabled webhook for `tag` plus its allowed event set,
    /// if any binding exists.
    async fn resolve_webhook(&self, tag: &str) -> Option<(nomifun_db::models::WebhookRow, Vec<String>)> {
        let setting = self.tag_settings.get(tag).await.ok().flatten()?;
        let events: Vec<String> = setting
            .notify_events
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let webhook_id = setting.webhook_id?;
        let webhook = self
            .webhooks
            .get_by_webhook_id(&webhook_id)
            .await
            .ok()
            .flatten()?;
        webhook.enabled.then_some((webhook, events))
    }
}

#[async_trait]
impl CompletionNotifier for CompletionNotifierImpl {
    async fn notify_completion(&self, requirement: &RequirementRow) {
        let Some((webhook, events)) = self.resolve_webhook(&requirement.tag).await else {
            return; // no binding / disabled / missing → silent skip
        };
        if !event_allowed(&requirement.status, &events) {
            return; // this event isn't in the tag's allowed set → silent skip
        }

        // Template: 【需求id】【需求名】【需求内容】【完成状态】【完成记录(报告)】
        let fields = vec![
            ("需求id".to_string(), requirement.id.to_string()),
            ("需求名".to_string(), requirement.title.clone()),
            (
                "需求内容".to_string(),
                truncate(&requirement.content, MAX_CONTENT_CHARS),
            ),
            ("完成状态".to_string(), status_label(&requirement.status).to_string()),
            (
                "完成记录(报告)".to_string(),
                requirement
                    .completion_note
                    .as_deref()
                    .map(|n| truncate(n, MAX_CONTENT_CHARS))
                    .unwrap_or_else(|| "-".to_string()),
            ),
        ];

        let title = format!("需求{}: {}", status_label(&requirement.status), requirement.title);
        if let Err(e) = self
            .sender
            .send_card(
                WebhookPlatform::from_db(&webhook.platform),
                &webhook.url,
                webhook.secret.as_deref(),
                &title,
                &fields,
            )
            .await
        {
            // Best-effort: log + swallow. A failing webhook must never affect
            // requirement state (and this runs on a detached task anyway).
            tracing::warn!(
                webhook_id = %webhook.webhook_id,
                requirement_id = %requirement.id,
                error = %e,
                "completion webhook delivery failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::event_allowed;
    #[test]
    fn allows_when_status_in_set() {
        assert!(event_allowed("done", &["done".to_string(), "failed".to_string()]));
        assert!(!event_allowed("needs_review", &["done".to_string(), "failed".to_string()]));
    }
    #[test]
    fn empty_set_allows_nothing() {
        assert!(!event_allowed("done", &[]));
    }
}

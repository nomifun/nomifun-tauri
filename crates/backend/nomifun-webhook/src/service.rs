//! Business logic for webhook CRUD + per-tag settings. No axum imports.

use std::sync::Arc;

use nomifun_api_types::{
    CreateWebhookRequest, TagSetting, UpdateWebhookRequest, UpsertTagSettingRequest, Webhook,
    WebhookId, WebhookPlatform,
};
use nomifun_common::{AppError, now_ms};
use nomifun_db::models::{TagSettingRow, WebhookRow};
use nomifun_db::{ITagSettingRepository, IWebhookRepository};

use crate::sender::WebhookSender;

/// Map a DB row to the client DTO (dropping the secret; exposing `has_secret`).
fn row_to_dto(row: &WebhookRow) -> Webhook {
    Webhook {
        webhook_id: WebhookId::parse(&row.webhook_id)
            .expect("repository returned an invalid webhook business id"),
        name: row.name.clone(),
        platform: WebhookPlatform::from_db(&row.platform),
        url: row.url.clone(),
        description: row.description.clone(),
        has_secret: row.secret.as_deref().is_some_and(|s| !s.is_empty()),
        enabled: row.enabled,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn tag_setting_to_dto(row: &TagSettingRow) -> TagSetting {
    TagSetting {
        tag: row.tag.clone(),
        webhook_id: row
            .webhook_id
            .as_deref()
            .map(WebhookId::parse)
            .transpose()
            .expect("repository returned an invalid tag-setting webhook id"),
        description: row.description.clone(),
        notify_events: row.notify_events.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect(),
    }
}

/// Default notification event set (all three), matching the column default and
/// the historical "fire on every completion transition" behavior.
fn default_events() -> Vec<String> {
    vec!["done".into(), "failed".into(), "needs_review".into()]
}

#[derive(Clone)]
pub struct WebhookService {
    webhooks: Arc<dyn IWebhookRepository>,
    tag_settings: Arc<dyn ITagSettingRepository>,
    sender: Arc<dyn WebhookSender>,
}

impl WebhookService {
    pub fn new(
        webhooks: Arc<dyn IWebhookRepository>,
        tag_settings: Arc<dyn ITagSettingRepository>,
        sender: Arc<dyn WebhookSender>,
    ) -> Self {
        Self {
            webhooks,
            tag_settings,
            sender,
        }
    }

    // ── Webhook CRUD ────────────────────────────────────────────────

    pub async fn list(&self) -> Result<Vec<Webhook>, AppError> {
        let rows = self.webhooks.list_all().await?;
        Ok(rows.iter().map(row_to_dto).collect())
    }

    pub async fn get(&self, webhook_id: &WebhookId) -> Result<Webhook, AppError> {
        let row = self
            .webhooks
            .get_by_webhook_id(webhook_id.as_str())
            .await?
            .ok_or_else(|| AppError::NotFound(format!("webhook {webhook_id}")))?;
        Ok(row_to_dto(&row))
    }

    pub async fn create(&self, req: CreateWebhookRequest) -> Result<Webhook, AppError> {
        if req.name.trim().is_empty() {
            return Err(AppError::BadRequest("name must not be empty".into()));
        }
        if req.url.trim().is_empty() {
            return Err(AppError::BadRequest("url must not be empty".into()));
        }
        let now = now_ms();
        let row = WebhookRow {
            webhook_id: WebhookId::new().into_string(),
            name: req.name,
            platform: req.platform.as_db().to_string(),
            url: req.url,
            secret: req.secret.filter(|s| !s.is_empty()),
            description: req.description,
            enabled: req.enabled.unwrap_or(true),
            created_at: now,
            updated_at: now,
        };
        let row = self.webhooks.insert(&row).await?;
        Ok(row_to_dto(&row))
    }

    pub async fn update(&self, webhook_id: &WebhookId, req: UpdateWebhookRequest) -> Result<Webhook, AppError> {
        let mut row = self
            .webhooks
            .get_by_webhook_id(webhook_id.as_str())
            .await?
            .ok_or_else(|| AppError::NotFound(format!("webhook {webhook_id}")))?;
        if let Some(name) = req.name {
            if name.trim().is_empty() {
                return Err(AppError::BadRequest("name must not be empty".into()));
            }
            row.name = name;
        }
        if let Some(url) = req.url {
            if url.trim().is_empty() {
                return Err(AppError::BadRequest("url must not be empty".into()));
            }
            row.url = url;
        }
        if let Some(platform) = req.platform {
            row.platform = platform.as_db().to_string();
        }
        if let Some(description) = req.description {
            row.description = description;
        }
        // `Some(Some(v))` sets, `Some(None)` clears, `None` keeps current.
        if let Some(secret) = req.secret {
            row.secret = secret.filter(|s| !s.is_empty());
        }
        if let Some(enabled) = req.enabled {
            row.enabled = enabled;
        }
        row.updated_at = now_ms();
        self.webhooks.update(&row).await?;
        Ok(row_to_dto(&row))
    }

    pub async fn delete(&self, webhook_id: &WebhookId) -> Result<(), AppError> {
        self.webhooks.delete(webhook_id.as_str()).await?;
        Ok(())
    }

    /// Send a sample card to verify the endpoint works. Surfaces send errors to
    /// the caller as a 502 (this is the only path that exposes webhook errors).
    pub async fn test(&self, webhook_id: &WebhookId) -> Result<(), AppError> {
        let row = self
            .webhooks
            .get_by_webhook_id(webhook_id.as_str())
            .await?
            .ok_or_else(|| AppError::NotFound(format!("webhook {webhook_id}")))?;
        let fields = vec![
            ("Nomi".to_string(), "Webhook test message".to_string()),
            ("Webhook".to_string(), row.name.clone()),
        ];
        self.sender
            .send_card(
                WebhookPlatform::from_db(&row.platform),
                &row.url,
                row.secret.as_deref(),
                "Nomi Webhook Test",
                &fields,
            )
            .await
            .map_err(|e| AppError::BadGateway(e.to_string()))
    }

    // ── Tag settings ────────────────────────────────────────────────

    pub async fn get_tag_setting(&self, tag: &str) -> Result<TagSetting, AppError> {
        match self.tag_settings.get(tag).await? {
            Some(row) => Ok(tag_setting_to_dto(&row)),
            // A tag with no settings row yet → return an empty (unbound) default
            // so the client always gets a consistent shape.
            None => Ok(TagSetting {
                tag: tag.to_string(),
                webhook_id: None,
                description: String::new(),
                notify_events: default_events(),
            }),
        }
    }

    pub async fn list_tag_settings(&self) -> Result<Vec<TagSetting>, AppError> {
        let rows = self.tag_settings.list_all().await?;
        Ok(rows.iter().map(tag_setting_to_dto).collect())
    }

    pub async fn upsert_tag_setting(&self, tag: &str, req: UpsertTagSettingRequest) -> Result<TagSetting, AppError> {
        if tag.trim().is_empty() {
            return Err(AppError::BadRequest("tag must not be empty".into()));
        }
        // Merge onto the existing row so a partial update keeps other fields.
        let existing = self.tag_settings.get(tag).await?;
        let webhook_id = match req.webhook_id {
            Some(v) => v, // Some(Some)=bind, Some(None)=clear
            None => existing
                .as_ref()
                .and_then(|r| r.webhook_id.as_deref())
                .map(WebhookId::parse)
                .transpose()
                .map_err(|error| AppError::Internal(format!(
                    "invalid persisted webhook_id for tag '{tag}': {error}"
                )))?,
        };
        // If binding a webhook, verify it exists (clean 400 vs a dangling id).
        if let Some(wh_id) = webhook_id.as_ref()
            && self
                .webhooks
                .get_by_webhook_id(wh_id.as_str())
                .await?
                .is_none()
        {
            return Err(AppError::BadRequest(format!("webhook {wh_id} does not exist")));
        }
        let description = req
            .description
            .or_else(|| existing.as_ref().map(|r| r.description.clone()))
            .unwrap_or_default();
        let events = req
            .notify_events
            .or_else(|| {
                existing
                    .as_ref()
                    .map(|r| r.notify_events.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect())
            })
            .unwrap_or_else(default_events);
        let row = TagSettingRow {
            tag: tag.to_string(),
            webhook_id: webhook_id.map(WebhookId::into_string),
            description,
            notify_events: events.join(","),
            updated_at: now_ms(),
        };
        self.tag_settings.upsert(&row).await?;
        Ok(tag_setting_to_dto(&row))
    }
}

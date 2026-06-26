use serde::{Deserialize, Deserializer, Serialize};

use crate::requirement::{AutoWorkRunState, AutoWorkTargetKind};

/// Deserialize a present field (including explicit `null`) into `Some(_)`, so an
/// absent field is `None` (keep) while `null` is `Some(None)` (clear). Without
/// this, serde collapses `null` to the outer `None`, making "clear" impossible.
///
/// Shared with [`crate::orchestrator`] for its fleet/workspace patch DTOs; keep
/// this the single source of truth so the patch semantics never drift.
pub(crate) fn double_option<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

/// Outbound webhook platform. Lark/飞书 custom bot, generic HTTP JSON, or Slack
/// incoming webhook. （钉钉等其他平台暂不支持。）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WebhookPlatform {
    #[default]
    Lark,
    Http,
    Slack,
}

impl WebhookPlatform {
    pub fn as_db(&self) -> &'static str {
        match self {
            Self::Lark => "lark",
            Self::Http => "http",
            Self::Slack => "slack",
        }
    }

    /// Parse from a DB string; unknown values fall back to `Lark`.
    pub fn from_db(s: &str) -> Self {
        match s {
            "http" => Self::Http,
            "slack" => Self::Slack,
            "lark" => Self::Lark,
            _ => Self::Lark,
        }
    }
}

/// A webhook as returned to clients. The signing `secret` is NEVER echoed back;
/// `has_secret` indicates whether one is stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Webhook {
    pub id: i64,
    pub name: String,
    pub platform: WebhookPlatform,
    pub url: String,
    pub description: String,
    pub has_secret: bool,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateWebhookRequest {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub platform: WebhookPlatform,
    #[serde(default)]
    pub description: String,
    /// Optional Lark signing secret (加签).
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Partial update. `secret` uses `Option<Option<String>>`: outer = "change?",
/// inner = `Some(v)` to set, `None` to clear.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateWebhookRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub platform: Option<WebhookPlatform>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub secret: Option<Option<String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Per-tag settings (bound webhook + description) over the implicit tags.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagSetting {
    pub tag: String,
    pub webhook_id: Option<i64>,
    pub description: String,
    /// Which completion events fire the bound webhook. Subset of
    /// `done`/`failed`/`needs_review`; empty means "never notify".
    pub notify_events: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpsertTagSettingRequest {
    /// `Some(Some(id))` binds, `Some(None)` clears, `None` keeps current.
    #[serde(default, deserialize_with = "double_option")]
    pub webhook_id: Option<Option<i64>>,
    #[serde(default)]
    pub description: Option<String>,
    /// `None` keeps the current set; `Some(events)` replaces it.
    #[serde(default)]
    pub notify_events: Option<Vec<String>>,
}

/// One session bound to a tag via its AutoWork config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagBinding {
    pub kind: AutoWorkTargetKind,
    pub target_id: String,
    pub name: String,
    pub run_state: AutoWorkRunState,
}

/// All AutoWork bindings for one tag (sessions whose autowork is enabled and
/// points at this tag), used by the AutoWork admin's 标签会话管理 tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagBindings {
    pub tag: String,
    pub bindings: Vec<TagBinding>,
}

//! Sidecar backup-model caller. Resolves the effective bypass provider/model
//! (per-watch override → global default in `client_preferences` → the session's
//! own model), then runs a one-shot completion and parses the strict-JSON
//! decision (with one retry).
//!
//! The provider call is behind the `Completer` trait so the supervisor tests can
//! inject canned responses without a live provider; the production impl wraps
//! `nomifun_ai_agent::{resolve_provider_config, one_shot_completion}`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use nomifun_ai_agent::{one_shot_completion, resolve_provider_config, user_message};
use nomifun_api_types::{BypassModelRef, DecisionStrategy};
use nomifun_db::{IClientPreferenceRepository, IProviderRepository};

use crate::prompt::{SIDECAR_SYSTEM, SidecarDecision, build_open_question_prompt, build_user_prompt, parse_decision};
use crate::signal::StallClass;

/// Global-default preference keys (stored in `client_preferences`).
pub const PREF_BACKUP_PROVIDER: &str = "idmm_backup_provider_id";
pub const PREF_BACKUP_MODEL: &str = "idmm_backup_model";
pub const PREF_DEFAULT_STEERING: &str = "idmm_default_steering_prompt";

const SIDECAR_MAX_TOKENS: u32 = 1024;

/// The provider call seam. Production wraps the real provider; tests inject.
#[async_trait]
pub trait Completer: Send + Sync {
    /// Run a system+user completion against `provider_id`/`model`. Returns the
    /// assembled text, or `Err(())` on any provider failure (→ rule fallback).
    async fn complete(&self, provider_id: &str, model: &str, system: &str, user: &str) -> Result<String, ()>;
}

/// Production completer: provider row → nomi Config → one-shot completion.
pub struct LiveCompleter {
    pub provider_repo: Arc<dyn IProviderRepository>,
    pub encryption_key: [u8; 32],
    pub workspace: PathBuf,
}

#[async_trait]
impl Completer for LiveCompleter {
    async fn complete(&self, provider_id: &str, model: &str, system: &str, user: &str) -> Result<String, ()> {
        let cfg = resolve_provider_config(
            &self.provider_repo,
            &self.encryption_key,
            provider_id,
            model,
            &self.workspace,
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "IDMM sidecar provider config resolution failed");
        })?;
        one_shot_completion(&cfg, system, vec![user_message(user)], SIDECAR_MAX_TOKENS)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "IDMM sidecar completion failed");
            })
    }
}

/// Outcome of a sidecar decision attempt.
#[derive(Debug, Clone)]
pub struct SidecarOutcome {
    /// The parsed decision, if the model produced valid JSON.
    pub decision: Option<SidecarDecision>,
    /// True if the provider call itself failed (vs. produced unparseable text).
    pub provider_failed: bool,
    /// The `(provider_id, model)` the sidecar resolved and used (or attempted,
    /// on a provider failure). `None` only when no backup was resolvable at all.
    /// Lets the caller record the audit `bypass_model` without re-resolving.
    pub resolved: Option<(String, String)>,
}

/// An open-question answer request (D6): the question text + its char cap. When
/// present, [`SidecarClient::decide`] uses the free-text answer prompt instead
/// of the option/permission prompt.
pub struct OpenQuestionAsk<'a> {
    pub question: &'a str,
    pub max_answer_chars: u32,
}

/// Resolves the bypass model and runs sidecar decisions.
pub struct SidecarClient {
    completer: Arc<dyn Completer>,
    client_prefs: Arc<dyn IClientPreferenceRepository>,
}

impl SidecarClient {
    pub fn new(completer: Arc<dyn Completer>, client_prefs: Arc<dyn IClientPreferenceRepository>) -> Self {
        Self {
            completer,
            client_prefs,
        }
    }

    /// Read a single global-default preference value.
    async fn pref(&self, key: &str) -> Option<String> {
        self.client_prefs
            .get_by_keys(&[key])
            .await
            .ok()
            .and_then(|rows| rows.into_iter().next())
            .map(|p| p.value)
    }

    async fn strategy_with_default_steering(&self, strategy: &DecisionStrategy) -> DecisionStrategy {
        if strategy
            .freeform_policy
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
        {
            return strategy.clone();
        }
        let Some(default_policy) = self
            .pref(PREF_DEFAULT_STEERING)
            .await
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        else {
            return strategy.clone();
        };
        let mut merged = strategy.clone();
        merged.freeform_policy = Some(default_policy);
        merged
    }

    /// Resolve effective `(provider_id, model)` from the watch's `bypass_model` +
    /// global defaults. `model` falls back to the global default, then to empty
    /// (the provider's default).
    pub async fn resolve_backup(&self, bypass: &BypassModelRef) -> Option<(String, String)> {
        let provider_id = match &bypass.provider_id {
            Some(p) if !p.is_empty() => p.clone(),
            _ => self.pref(PREF_BACKUP_PROVIDER).await?,
        };
        let model = match &bypass.model {
            Some(m) if !m.is_empty() => m.clone(),
            _ => self.pref(PREF_BACKUP_MODEL).await.unwrap_or_default(),
        };
        Some((provider_id, model))
    }

    /// Whether a backup provider is resolvable for this watch's bypass model —
    /// used by validation + the `sidecar_provider_resolved` state flag.
    pub async fn backup_resolvable(&self, bypass: &BypassModelRef) -> bool {
        self.resolve_backup(bypass).await.is_some()
    }

    /// Run one sidecar decision pass.
    ///
    /// `bypass` is the active watch's bypass-model selection (per-watch override →
    /// global default). `strategy` drives the prompt's policy block (tendency /
    /// freeform / never-destructive). `fallback` is the supervised session's own
    /// `(provider_id, model)` — used when no per-watch/global backup is
    /// configured, so the model tier works out-of-the-box on a plain desktop chat
    /// (the session's own model becomes the bypass model). `open_question`, when
    /// `Some`, switches to the free-text answer prompt (D6).
    #[allow(clippy::too_many_arguments)]
    pub async fn decide(
        &self,
        bypass: &BypassModelRef,
        strategy: &DecisionStrategy,
        class: StallClass,
        detail: &str,
        context: &str,
        fallback: Option<(String, String)>,
        open_question: Option<OpenQuestionAsk<'_>>,
    ) -> SidecarOutcome {
        let resolved = match self.resolve_backup(bypass).await {
            Some(pm) => Some(pm),
            None => fallback.filter(|(p, _)| !p.trim().is_empty()),
        };
        let Some((provider_id, model)) = resolved else {
            return SidecarOutcome {
                decision: None,
                provider_failed: true,
                resolved: None,
            };
        };
        let used = (provider_id.clone(), model.clone());
        let effective_strategy = self.strategy_with_default_steering(strategy).await;
        let user = match &open_question {
            Some(oq) => build_open_question_prompt(&effective_strategy, oq.question, context, oq.max_answer_chars),
            None => build_user_prompt(&effective_strategy, class, detail, context),
        };

        // First attempt.
        let raw = match self
            .completer
            .complete(&provider_id, &model, SIDECAR_SYSTEM, &user)
            .await
        {
            Ok(r) => r,
            Err(()) => {
                return SidecarOutcome {
                    decision: None,
                    provider_failed: true,
                    resolved: Some(used),
                };
            }
        };
        if let Some(d) = parse_decision(&raw) {
            return SidecarOutcome {
                decision: Some(d),
                provider_failed: false,
                resolved: Some(used),
            };
        }

        // One retry, nudging for strict JSON.
        let retry_user = format!("{user}\n\nReturn ONLY the JSON object, nothing else.");
        match self
            .completer
            .complete(&provider_id, &model, SIDECAR_SYSTEM, &retry_user)
            .await
        {
            Ok(r2) => SidecarOutcome {
                decision: parse_decision(&r2),
                provider_failed: false,
                resolved: Some(used),
            },
            Err(()) => SidecarOutcome {
                decision: None,
                provider_failed: true,
                resolved: Some(used),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::{BypassModelRef, DecisionStrategy};
    use nomifun_db::DbError;
    use nomifun_db::models::ClientPreference;
    use std::sync::Mutex;

    // ── Mock client-preferences repo ──
    #[derive(Default)]
    struct MockPrefs {
        map: Mutex<std::collections::HashMap<String, String>>,
    }
    impl MockPrefs {
        fn with(pairs: &[(&str, &str)]) -> Self {
            let m = Self::default();
            for (k, v) in pairs {
                m.map.lock().unwrap().insert(k.to_string(), v.to_string());
            }
            m
        }
    }
    #[async_trait]
    impl IClientPreferenceRepository for MockPrefs {
        async fn get_all(&self) -> Result<Vec<ClientPreference>, DbError> {
            Ok(vec![])
        }
        async fn get_by_keys(&self, keys: &[&str]) -> Result<Vec<ClientPreference>, DbError> {
            let map = self.map.lock().unwrap();
            Ok(keys
                .iter()
                .filter_map(|k| {
                    map.get(*k).map(|v| ClientPreference {
                        key: k.to_string(),
                        value: v.clone(),
                        updated_at: 0,
                    })
                })
                .collect())
        }
        async fn upsert_batch(&self, entries: &[(&str, &str)]) -> Result<(), DbError> {
            let mut map = self.map.lock().unwrap();
            for (k, v) in entries {
                map.insert(k.to_string(), v.to_string());
            }
            Ok(())
        }
        async fn delete_keys(&self, keys: &[&str]) -> Result<(), DbError> {
            let mut map = self.map.lock().unwrap();
            for k in keys {
                map.remove(*k);
            }
            Ok(())
        }
    }

    // ── Mock completer: scripted responses ──
    struct ScriptedCompleter {
        responses: Mutex<Vec<Result<String, ()>>>,
        calls: Mutex<u32>,
    }
    impl ScriptedCompleter {
        fn new(responses: Vec<Result<String, ()>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: Mutex::new(0),
            }
        }
    }

    struct CapturingCompleter {
        last_user: Mutex<Option<String>>,
    }

    #[async_trait]
    impl Completer for CapturingCompleter {
        async fn complete(&self, _p: &str, _m: &str, _s: &str, user: &str) -> Result<String, ()> {
            *self.last_user.lock().unwrap() = Some(user.to_string());
            Ok(r#"{"action":"retry","confidence":0.9}"#.into())
        }
    }

    #[async_trait]
    impl Completer for ScriptedCompleter {
        async fn complete(&self, _p: &str, _m: &str, _s: &str, _u: &str) -> Result<String, ()> {
            *self.calls.lock().unwrap() += 1;
            let mut r = self.responses.lock().unwrap();
            if r.is_empty() { Err(()) } else { r.remove(0) }
        }
    }

    fn bypass() -> BypassModelRef {
        BypassModelRef {
            provider_id: Some("prov1".into()),
            model: Some("m1".into()),
        }
    }

    fn strat() -> DecisionStrategy {
        DecisionStrategy::default()
    }

    #[tokio::test]
    async fn resolve_backup_prefers_watch_then_global() {
        let prefs = Arc::new(MockPrefs::with(&[
            (PREF_BACKUP_PROVIDER, "global_prov"),
            (PREF_BACKUP_MODEL, "global_model"),
        ]));
        let comp = Arc::new(ScriptedCompleter::new(vec![]));
        let client = SidecarClient::new(comp, prefs);

        // Per-watch override wins.
        let watch = BypassModelRef {
            provider_id: Some("watch_prov".into()),
            model: Some("watch_model".into()),
        };
        assert_eq!(
            client.resolve_backup(&watch).await,
            Some(("watch_prov".into(), "watch_model".into()))
        );

        // Empty → global default.
        let empty = BypassModelRef::default();
        assert_eq!(
            client.resolve_backup(&empty).await,
            Some(("global_prov".into(), "global_model".into()))
        );
    }

    #[tokio::test]
    async fn resolve_backup_none_when_no_provider_anywhere() {
        let prefs = Arc::new(MockPrefs::default());
        let comp = Arc::new(ScriptedCompleter::new(vec![]));
        let client = SidecarClient::new(comp, prefs);
        assert!(client.resolve_backup(&BypassModelRef::default()).await.is_none());
        assert!(!client.backup_resolvable(&BypassModelRef::default()).await);
    }

    #[tokio::test]
    async fn sidecar_uses_global_default_steering_when_watch_policy_empty() {
        let prefs = Arc::new(MockPrefs::with(&[
            (PREF_BACKUP_PROVIDER, "global_prov"),
            (PREF_DEFAULT_STEERING, "prefer option 2 when safe"),
        ]));
        let comp = Arc::new(CapturingCompleter { last_user: Mutex::new(None) });
        let client = SidecarClient::new(comp.clone(), prefs);

        let out = client
            .decide(
                &BypassModelRef::default(),
                &DecisionStrategy::default(),
                StallClass::Decision,
                "pick an option",
                "ctx",
                None,
                None,
            )
            .await;

        assert!(!out.provider_failed);
        let prompt = comp.last_user.lock().unwrap().clone().unwrap();
        assert!(
            prompt.contains("prefer option 2 when safe"),
            "global default steering prompt must be applied to an empty per-watch policy; prompt was: {prompt}"
        );
    }

    #[tokio::test]
    async fn sidecar_returns_parsed_decision() {
        let prefs = Arc::new(MockPrefs::default());
        let comp = Arc::new(ScriptedCompleter::new(vec![Ok(
            r#"{"action":"retry","confidence":0.9,"reason":"transient"}"#.into(),
        )]));
        let client = SidecarClient::new(comp, prefs);
        let out = client
            .decide(&bypass(), &strat(), StallClass::ProviderError, "500", "ctx", None, None)
            .await;
        assert!(!out.provider_failed);
        assert_eq!(out.decision.unwrap().action, "retry");
    }

    #[tokio::test]
    async fn sidecar_retries_once_on_garbage_then_parses() {
        let prefs = Arc::new(MockPrefs::default());
        let comp = Arc::new(ScriptedCompleter::new(vec![
            Ok("sorry, I cannot".into()),
            Ok(r#"{"action":"send_text","text":"continue"}"#.into()),
        ]));
        let client = SidecarClient::new(comp.clone(), prefs);
        let out = client
            .decide(&bypass(), &strat(), StallClass::Idle, "idle", "ctx", None, None)
            .await;
        assert!(!out.provider_failed);
        assert_eq!(out.decision.unwrap().action, "send_text");
        assert_eq!(*comp.calls.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn sidecar_garbage_twice_yields_no_decision() {
        let prefs = Arc::new(MockPrefs::default());
        let comp = Arc::new(ScriptedCompleter::new(vec![Ok("nope".into()), Ok("still nope".into())]));
        let client = SidecarClient::new(comp, prefs);
        let out = client
            .decide(&bypass(), &strat(), StallClass::Idle, "idle", "ctx", None, None)
            .await;
        assert!(!out.provider_failed);
        assert!(out.decision.is_none());
    }

    #[tokio::test]
    async fn sidecar_provider_error_sets_provider_failed() {
        let prefs = Arc::new(MockPrefs::default());
        let comp = Arc::new(ScriptedCompleter::new(vec![Err(())]));
        let client = SidecarClient::new(comp, prefs);
        let out = client
            .decide(&bypass(), &strat(), StallClass::ProviderError, "500", "ctx", None, None)
            .await;
        assert!(out.provider_failed);
        assert!(out.decision.is_none());
    }

    #[tokio::test]
    async fn sidecar_open_question_returns_answer_text() {
        // D6: an open-question ask uses the free-text prompt and the model
        // replies with answer_text.
        let prefs = Arc::new(MockPrefs::default());
        let comp = Arc::new(ScriptedCompleter::new(vec![Ok(
            r#"{"action":"answer_text","text":"用 LRU + 30 分钟 TTL","confidence":0.8}"#.into(),
        )]));
        let client = SidecarClient::new(comp, prefs);
        let out = client
            .decide(
                &bypass(),
                &strat(),
                StallClass::OpenQuestion,
                "open question: 缓存怎么设计",
                "ctx",
                None,
                Some(OpenQuestionAsk {
                    question: "你希望缓存怎么设计？",
                    max_answer_chars: 600,
                }),
            )
            .await;
        assert!(!out.provider_failed);
        let d = out.decision.unwrap();
        assert_eq!(d.action, "answer_text");
        assert_eq!(d.text, "用 LRU + 30 分钟 TTL");
    }
}

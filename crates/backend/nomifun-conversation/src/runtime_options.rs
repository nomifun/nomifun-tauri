//! Shared helpers for translating a [`ConversationRow`] into Agent runtime
//! construction inputs.
//!
//! Every Agent-runtime entry point — interactive `send_message`, cron, and
//! AutoWork — must derive the same typed conversation settings for a
//! given conversation; otherwise a Nomi job that runs fine
//! interactively can fail under cron with `Provider '<vendor>' not
//! found` (Sentry ELECTRON-1HM). Centralising the lookup here forces
//! both paths through one parser.
//!
//! The model column has one canonical JSON shape. Missing selections remain
//! `None`; malformed persisted values are errors and never degrade into an
//! empty provider/model sentinel.

use nomifun_common::{AppError, DelegationPolicy, ProviderWithModel};
use nomifun_db::models::ConversationRow;

use crate::convert::parse_provider_with_model;

/// Parse the authoritative conversation-level delegation policy for runtime
/// construction. Unknown persisted values are rejected rather than silently
/// widening Agent capabilities.
pub fn delegation_policy_from_conversation_row(row: &ConversationRow) -> Result<DelegationPolicy, AppError> {
    row.delegation_policy
        .parse()
        .map_err(|error| AppError::Internal(format!("Invalid conversation delegation policy: {error}")))
}

/// Resolve a conversation row's canonical stored model.
///
/// `NULL` means no conversation-level model (valid for backends such as ACP).
/// Invalid JSON, legacy field aliases, malformed provider IDs, and incomplete
/// model references are rejected as corrupt persisted state.
pub fn provider_model_from_conversation_row(
    row: &ConversationRow,
) -> Result<Option<ProviderWithModel>, AppError> {
    row.model
        .as_deref()
        .map(parse_provider_with_model)
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::ConversationId;

    const PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";

    fn row_with_model(model: Option<&str>) -> ConversationRow {
        let conversation_id = ConversationId::new().into_string();
        ConversationRow {
            id: 0,
            conversation_id,
            user_id: "user-1".into(),
            name: "test".into(),
            r#type: "nomi".into(),
            model: model.map(ToOwned::to_owned),
            extra: "{}".into(),
            delegation_policy: "automatic".into(),
            execution_model_pool: None,
            decision_policy: "automatic".into(),
            execution_template_id: None,
            status: None,
            source: None,
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            cron_job_id: None,
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn parses_canonical_shape() {
        let json = format!(
            r#"{{"provider_id":"{PROVIDER_ID}","model":"gpt-5","use_model":"gpt-5-turbo"}}"#
        );
        let row = row_with_model(Some(&json));
        let m = provider_model_from_conversation_row(&row).unwrap().unwrap();
        assert_eq!(m.provider_id, PROVIDER_ID);
        assert_eq!(m.model, "gpt-5");
        assert_eq!(m.use_model.as_deref(), Some("gpt-5-turbo"));
    }

    #[test]
    fn rejects_camelcase_legacy_shape() {
        let json = format!(r#"{{"providerId":"{PROVIDER_ID}","model":"gpt-5"}}"#);
        let row = row_with_model(Some(&json));
        assert!(provider_model_from_conversation_row(&row).is_err());
    }

    #[test]
    fn rejects_id_alias() {
        let json = format!(r#"{{"id":"{PROVIDER_ID}","model":"gpt-5"}}"#);
        let row = row_with_model(Some(&json));
        assert!(provider_model_from_conversation_row(&row).is_err());
    }

    #[test]
    fn null_model_returns_none() {
        let row = row_with_model(None);
        assert!(provider_model_from_conversation_row(&row).unwrap().is_none());
    }

    #[test]
    fn invalid_json_is_rejected() {
        let row = row_with_model(Some("not-json"));
        assert!(provider_model_from_conversation_row(&row).is_err());
    }

    #[test]
    fn missing_provider_id_is_rejected() {
        let json = r#"{"model":"gpt-5"}"#;
        let row = row_with_model(Some(json));
        assert!(provider_model_from_conversation_row(&row).is_err());
    }

    #[test]
    fn typed_delegation_policy_is_parsed_from_conversation_column() {
        let mut row = row_with_model(None);
        row.delegation_policy = "prefer_parallel".into();
        assert_eq!(
            delegation_policy_from_conversation_row(&row).unwrap(),
            DelegationPolicy::PreferParallel
        );
    }

    #[test]
    fn unknown_delegation_policy_is_rejected() {
        let mut row = row_with_model(None);
        row.delegation_policy = "unbounded".into();
        assert!(delegation_policy_from_conversation_row(&row).is_err());
    }

    /// Regression: the interactive `send_message` path and the cron
    /// executor must derive the same `(provider_id, model)` for a given
    /// conversation. Before this helper existed, cron read
    /// `agent_config.backend` (which fell back to the literal vendor
    /// label `"nomi"` when the conversation's model JSON was an older
    /// shape) and `send_message` parsed the row directly, so the cron
    /// path would emit `Provider 'nomi' not found` while the
    /// interactive path used the real provider hash. Now both paths
    /// route through `provider_model_from_conversation_row` and must
    /// agree on the one canonical row shape.
    #[test]
    fn interactive_and_cron_paths_agree_on_provider_id() {
        let canonical = format!(
            r#"{{"provider_id":"{PROVIDER_ID}","model":"gpt-5","use_model":null}}"#
        );

        let canonical_row = row_with_model(Some(&canonical));

        let canonical_resolved = provider_model_from_conversation_row(&canonical_row)
            .unwrap()
            .unwrap();

        assert_eq!(canonical_resolved.provider_id, PROVIDER_ID);
        assert_ne!(canonical_resolved.provider_id, "nomi");
    }
}

//! IDMM (Intelligent Decision-Making Mode) capabilities (registry form):
//! read + set the per-session supervision config for a conversation or terminal
//! target.
//!
//! Clean migration of `tools_idmm.rs` onto the capability registry. The typed
//! params structs are now the single source (schema + runtime deserialization).
//! Handler logic is identical to the legacy: overlay onto the previously
//! persisted config so unexposed knobs (fault watch / strategy / budget) keep
//! their values; the gateway exposes the DECISION watch's core knobs
//! (enabled / tier / freeform policy).

use std::sync::Arc;

use nomifun_api_types::{IdmmTargetKind, WatchTier};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::{CallerCtx, GatewayDeps};
use crate::id_schema::{CanonicalEntityId, SessionTargetKind};
use crate::registry::{Capability, CapabilityMeta, DangerTier};
use crate::server::{ok, require_user};

// ─── Params ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetIdmmParams {
    /// Target kind: "conversation" or "terminal".
    kind: SessionTargetKind,
    /// The conversation id or terminal id to supervise.
    target_id: CanonicalEntityId,
    /// Enable (true) or disable (false) IDMM supervision.
    enabled: bool,
    /// Escalation tier: "rule_only" (no-model rules) or
    /// "rule_plus_model" (rules plus a backup-model sidecar).
    #[serde(default)]
    tier: Option<WatchTierParam>,
    /// Bounds what the sidecar may decide on the user's behalf. Required
    /// (non-empty) for the rule_plus_model tier.
    #[serde(default)]
    steering_prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WatchTierParam {
    RuleOnly,
    RulePlusModel,
}

impl From<WatchTierParam> for WatchTier {
    fn from(value: WatchTierParam) -> Self {
        match value {
            WatchTierParam::RuleOnly => Self::RuleOnly,
            WatchTierParam::RulePlusModel => Self::RulePlusModel,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetIdmmParams {
    /// Target kind: "conversation" or "terminal".
    kind: SessionTargetKind,
    /// The conversation id or terminal id to inspect.
    target_id: CanonicalEntityId,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

pub(crate) fn parse_target_id(kind: IdmmTargetKind, raw: String) -> Result<String, Value> {
    match kind {
        IdmmTargetKind::Conversation => nomifun_common::ConversationId::parse(raw)
            .map(nomifun_common::ConversationId::into_string)
            .map_err(|error| json!({ "error": format!("invalid conversation target_id: {error}") })),
        IdmmTargetKind::Terminal => nomifun_common::TerminalId::parse(raw)
            .map(nomifun_common::TerminalId::into_string)
            .map_err(|error| json!({ "error": format!("invalid terminal target_id: {error}") })),
    }
}

/// Shared Gateway ownership boundary for every target-scoped IDMM capability.
/// Keeping this in one helper prevents the base and extended capability sets
/// from drifting into different authorization behavior.
pub(crate) async fn verify_target(
    deps: &GatewayDeps,
    ctx: &CallerCtx,
    kind: IdmmTargetKind,
    target_id: &str,
) -> Option<Value> {
    let user_id = match require_user(ctx) {
        Ok(u) => u.to_owned(),
        Err(e) => return Some(e),
    };
    match deps
        .idmm_service
        .verify_target_owner(kind, target_id, &user_id)
        .await
    {
        Ok(()) => None,
        Err(e) => Some(json!({"error": e.to_string()})),
    }
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn set(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: SetIdmmParams) -> Value {
    let kind = IdmmTargetKind::from(p.kind);
    let target_id = match parse_target_id(kind, p.target_id.into_string()) {
        Ok(target_id) => target_id,
        Err(error) => return error,
    };
    if let Some(err) = verify_target(&deps, &ctx, kind, &target_id).await {
        return err;
    }

    // Overlay onto the previously persisted config so unexposed knobs
    // (fault watch / strategy / budget details) keep their values. The gateway
    // exposes the DECISION watch (the agent-facing decision capability).
    let mut cfg = match deps
        .idmm_service
        .read_config_persisted(ctx.user_id.as_str(), kind, &target_id)
        .await
    {
        Ok(c) => c.unwrap_or_default(),
        Err(e) => return json!({"error": e.to_string()}),
    };
    cfg.decision_watch.base.enabled = p.enabled;
    if let Some(tier) = p.tier {
        cfg.decision_watch.base.tier = tier.into();
    }
    if let Some(sp) = p.steering_prompt {
        cfg.decision_watch.strategy.freeform_policy = Some(sp);
    }

    if let Err(e) = deps
        .idmm_service
        .save_config(ctx.user_id.as_str(), kind, &target_id, &cfg)
        .await
    {
        // Typical validation errors: sidecar tier without a steering prompt
        // or without a resolvable backup provider — relay them verbatim so
        // the agent can fix the call or ask the owner.
        return json!({"error": e.to_string()});
    }
    match deps
        .idmm_service
        .build_state(ctx.user_id.as_str(), kind, &target_id)
        .await
    {
        Ok(state) => ok(state),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn get(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: GetIdmmParams) -> Value {
    let kind = IdmmTargetKind::from(p.kind);
    let target_id = match parse_target_id(kind, p.target_id.into_string()) {
        Ok(target_id) => target_id,
        Err(error) => return error,
    };
    if let Some(err) = verify_target(&deps, &ctx, kind, &target_id).await {
        return err;
    }
    match deps
        .idmm_service
        .build_state(ctx.user_id.as_str(), kind, &target_id)
        .await
    {
        Ok(state) => ok(state),
        Err(e) => json!({"error": e.to_string()}),
    }
}

// ─── Registration ────────────────────────────────────────────────────────────

/// Register the IDMM-domain capabilities.
pub(crate) fn register(out: &mut Vec<Capability>) {
    out.push(Capability::new::<SetIdmmParams, _, _>(
        CapabilityMeta::new(
            "nomi_set_idmm",
            "idmm",
            "Update IDMM supervision knobs (enabled / tier / steering prompt) and (re)arm the live supervisor for a conversation or terminal.",
            DangerTier::Write,
        ),
        |deps, ctx, p| set(deps, ctx, p),
    ));
    out.push(Capability::new::<GetIdmmParams, _, _>(
        CapabilityMeta::new(
            "nomi_get_idmm",
            "idmm",
            "Read the current IDMM config and live supervision state for a conversation or terminal.",
            DangerTier::Read,
        ),
        |deps, ctx, p| get(deps, ctx, p),
    ));
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_kind_is_a_closed_enum() {
        let conversation: SessionTargetKind =
            serde_json::from_str(r#""conversation""#).expect("conversation is valid");
        let terminal: SessionTargetKind =
            serde_json::from_str(r#""terminal""#).expect("terminal is valid");
        assert!(matches!(conversation, SessionTargetKind::Conversation));
        assert!(matches!(terminal, SessionTargetKind::Terminal));
        assert!(serde_json::from_str::<SessionTargetKind>(r#""unknown""#).is_err());
    }

    #[test]
    fn tier_is_a_closed_enum_without_legacy_aliases() {
        assert!(matches!(
            serde_json::from_str::<WatchTierParam>(r#""rule_only""#).unwrap(),
            WatchTierParam::RuleOnly
        ));
        assert!(matches!(
            serde_json::from_str::<WatchTierParam>(r#""rule_plus_model""#).unwrap(),
            WatchTierParam::RulePlusModel
        ));
        for rejected in [r#""rule""#, r#""rule_plus_sidecar""#, r#""bogus""#] {
            assert!(serde_json::from_str::<WatchTierParam>(rejected).is_err());
        }
    }
}

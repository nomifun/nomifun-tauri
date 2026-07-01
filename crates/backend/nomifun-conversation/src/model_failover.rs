//! Phase 3 模型故障转移队列(spec §5.5)的纯逻辑层 + 配置读写。
//!
//! 故障转移**只能**在会话服务层(send loop)做:`NomiAgentManager` 不保留重建
//! 输入、engine 也无法原地换 provider,所以"换模型"等于改 `conversation.model` +
//! 杀任务、下次 send 重建。本模块只承担两件无副作用/低副作用的事:
//!
//! 1. [`next_failover_model`] —— 纯函数挑选器(D2),给定失败模型与队列,按序返回
//!    首个可用候选;跳过 provider 关停 / 模型禁用 / 健康检查标 Unhealthy / 失败本身;
//!    队列耗尽返回 `None`(send-loop 见 `None` 即按现状 emit 原始错误,绝不无限切换)。
//! 2. 配置读写 —— 全局存 `client_preferences` 键 `agent.model_failover`(整体 JSON,
//!    形状抄 `nomifun-idmm/service.rs` 的多字段 pref 先例),会话级可在
//!    `conversations.extra.model_failover` 覆盖(存在则优先于全局)。
//!
//! 健康字段 fail-open:`provider` 的 `model_enabled` / `model_health` 是 TEXT JSON,
//! 解析失败时按"未禁用 / 未知健康"处理 —— 宁可多保留一个候选也不要因脏数据把队列误清空。

use std::sync::Arc;

use nomifun_api_types::{AgentErrorCode, HealthStatus, ModelFailoverConfig};
use nomifun_common::{AppError, ErrorChain, ProviderWithModel};
use nomifun_db::IClientPreferenceRepository;
use nomifun_db::models::Provider;
use tracing::warn;

/// `client_preferences` 键,存放全局模型故障转移配置(整体 JSON)。
pub const MODEL_FAILOVER_PREF_KEY: &str = "agent.model_failover";

/// 判定一个 `AgentErrorCode` 是否为「provider 故障」——即换个备用模型可能绕过的
/// 单厂商失败(限流 / 5xx / 网络 / 配置)。
///
/// 这张 matches 表是 `nomifun-idmm/config.rs::is_provider_fault` 的**就地副本**:
/// 故障转移 seam 在 `nomifun-conversation`,而 `nomifun-idmm` 在其之上,直接依赖
/// 会形成倒置的 crate 边界。两份必须保持一致;改动其一时同步另一处。
pub fn is_provider_fault(code: AgentErrorCode) -> bool {
    use AgentErrorCode::*;
    matches!(
        code,
        UserLlmProviderAuthFailed
            | UserLlmProviderPermissionDenied
            | UserLlmProviderBillingRequired
            | UserLlmProviderConfigError
            | UserLlmProviderModelNotFound
            | UserLlmProviderUnsupportedModel
            | UserLlmProviderEndpointNotFound
            | UserLlmProviderInvalidRequest
            | UserLlmProviderInvalidToolSchema
            | UserLlmProviderContextTooLarge
            | UserLlmProviderRateLimited
            | UserLlmProviderTimeout
            | UserLlmProviderNetworkError
            | UserLlmProviderEmptyResponse
            | UserLlmProviderGatewayError
            | UnknownUpstreamError
    )
}

/// 读全局故障转移配置。未设置(无该 pref 行)或 JSON 损坏时回落到
/// [`ModelFailoverConfig::default`](默认关闭),保证调用方永远拿到可用配置。
pub async fn get_global_failover_config(
    client_prefs: &Arc<dyn IClientPreferenceRepository>,
) -> ModelFailoverConfig {
    let rows = match client_prefs.get_by_keys(&[MODEL_FAILOVER_PREF_KEY]).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = %ErrorChain(&e), "Failed to read model failover pref; defaulting to disabled");
            return ModelFailoverConfig::default();
        }
    };
    rows.into_iter()
        .find(|r| r.key == MODEL_FAILOVER_PREF_KEY)
        .and_then(|r| match serde_json::from_str::<ModelFailoverConfig>(&r.value) {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                warn!(error = %ErrorChain(&e), "Malformed model failover pref; defaulting to disabled");
                None
            }
        })
        .unwrap_or_default()
}

/// 写全局故障转移配置(整体 JSON 进单个 pref 键)。形状抄 idmm `set_settings` 的
/// `upsert_batch` 先例。
pub async fn set_global_failover_config(
    client_prefs: &Arc<dyn IClientPreferenceRepository>,
    config: &ModelFailoverConfig,
) -> Result<(), AppError> {
    let value =
        serde_json::to_string(config).map_err(|e| AppError::Internal(format!("serialize failover config: {e}")))?;
    client_prefs
        .upsert_batch(&[(MODEL_FAILOVER_PREF_KEY, value.as_str())])
        .await?;
    Ok(())
}

/// 从 `conversations.extra` 的 JSON 文本里读会话级覆盖。`extra.model_failover`
/// 存在(且能解析)则返回它,否则 `None`(交由调用方回落到全局)。脏 `extra` /
/// 缺字段一律按"无覆盖"处理 —— 与会话其余 extra 字段的容错读法一致。
pub fn read_conversation_failover_override(extra_json: &str) -> Option<ModelFailoverConfig> {
    let value: serde_json::Value = serde_json::from_str(extra_json).ok()?;
    let raw = value.get("model_failover")?;
    serde_json::from_value::<ModelFailoverConfig>(raw.clone()).ok()
}

/// 解析的可用性视图(从 [`Provider`] 的 TEXT JSON 字段抽出,fail-open)。
struct ProviderAvailability {
    enabled: bool,
    model_enabled: std::collections::HashMap<String, bool>,
    model_health: std::collections::HashMap<String, HealthStatus>,
}

impl ProviderAvailability {
    /// 解析一行 provider 的 `enabled` / `model_enabled` / `model_health`。JSON 字段
    /// 解析失败时退化为空映射(=未知/未禁用),保证脏数据不会误判候选不可用。
    fn from_provider(provider: &Provider) -> Self {
        let model_enabled = provider
            .model_enabled
            .as_deref()
            .and_then(|s| serde_json::from_str::<std::collections::HashMap<String, bool>>(s).ok())
            .unwrap_or_default();
        // 只取每个模型的 `status` 字段;其余健康元数据(last_check 等)与挑选无关。
        let model_health = provider
            .model_health
            .as_deref()
            .and_then(|s| {
                serde_json::from_str::<std::collections::HashMap<String, nomifun_api_types::ModelHealthStatus>>(s).ok()
            })
            .map(|m| m.into_iter().map(|(k, v)| (k, v.status)).collect())
            .unwrap_or_default();
        Self {
            enabled: provider.enabled,
            model_enabled,
            model_health,
        }
    }

    /// 该模型是否可作为候选:provider 启用 && 模型未被显式禁用 && 健康检查未标
    /// Unhealthy(Unknown / Healthy / 无记录都放行)。
    fn model_is_candidate(&self, model: &str) -> bool {
        if !self.enabled {
            return false;
        }
        if self.model_enabled.get(model) == Some(&false) {
            return false;
        }
        if self.model_health.get(model) == Some(&HealthStatus::Unhealthy) {
            return false;
        }
        true
    }
}

/// D2 挑选器(纯函数):按队列序返回首个可用候选模型。
///
/// 跳过:`provider.enabled == false`、`model_enabled[model] == Some(false)`、
/// `model_health[model].status == Unhealthy`、与刚失败的 `(provider_id, model)`
/// 完全相同的条目、以及**本轮已经试过**的任何 `(provider_id, model)`(`tried`,
/// review #2 单调性:多次切换时不回头重试已切过的候选,杜绝队列里循环抖动)。
/// 无可用候选时返回 `None`(队列耗尽 → send-loop 不再转移)。
///
/// `providers` 是当前全部 provider 行;队列里引用的 provider 若不在表中,该候选
/// 被视为不可用(找不到 = 不能用)。
pub fn next_failover_model(
    queue: &[ProviderWithModel],
    failed: &ProviderWithModel,
    tried: &[ProviderWithModel],
    providers: &[Provider],
) -> Option<ProviderWithModel> {
    let same = |a: &ProviderWithModel, b: &ProviderWithModel| a.provider_id == b.provider_id && a.model == b.model;
    queue.iter().find_map(|candidate| {
        // 跳过刚失败的同一 (provider_id, model)。
        if same(candidate, failed) {
            return None;
        }
        // review #2:跳过本轮已经切到过的候选(单调推进,不重试)。
        if tried.iter().any(|t| same(candidate, t)) {
            return None;
        }
        let provider = providers.iter().find(|p| p.id == candidate.provider_id)?;
        let availability = ProviderAvailability::from_provider(provider);
        if availability.model_is_candidate(&candidate.model) {
            Some(candidate.clone())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn pwm(provider_id: &str, model: &str) -> ProviderWithModel {
        ProviderWithModel {
            provider_id: provider_id.into(),
            model: model.into(),
            use_model: None,
        }
    }

    /// 构造一行 provider:`enabled` + 每模型启用/健康映射序列化进 TEXT JSON。
    fn provider(
        id: &str,
        enabled: bool,
        model_enabled: &[(&str, bool)],
        model_health: &[(&str, HealthStatus)],
    ) -> Provider {
        let enabled_map: HashMap<String, bool> =
            model_enabled.iter().map(|(m, e)| (m.to_string(), *e)).collect();
        let health_map: HashMap<String, nomifun_api_types::ModelHealthStatus> = model_health
            .iter()
            .map(|(m, s)| {
                (
                    m.to_string(),
                    nomifun_api_types::ModelHealthStatus {
                        status: *s,
                        last_check: None,
                        latency: None,
                        error: None,
                    },
                )
            })
            .collect();
        Provider {
            id: id.into(),
            platform: "openai".into(),
            name: id.into(),
            base_url: "https://example.com".into(),
            api_key_encrypted: "x".into(),
            models: "[]".into(),
            enabled,
            capabilities: "[]".into(),
            context_limit: None,
            model_protocols: None,
            model_descriptions: None,
            model_enabled: if enabled_map.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&enabled_map).unwrap())
            },
            model_health: if health_map.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&health_map).unwrap())
            },
            bedrock_config: None,
            is_full_url: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn picks_next_available_skipping_failed() {
        let queue = vec![pwm("p1", "m1"), pwm("p2", "m2")];
        let failed = pwm("p1", "m1");
        let providers = vec![provider("p1", true, &[], &[]), provider("p2", true, &[], &[])];
        let pick = next_failover_model(&queue, &failed, &[], &providers).expect("should pick p2/m2");
        assert_eq!(pick.provider_id, "p2");
        assert_eq!(pick.model, "m2");
    }

    #[test]
    fn skips_already_tried_candidates() {
        // review #2 (monotonicity): a candidate already switched to this turn is
        // skipped even though it is still healthy/enabled — so multiple failover
        // hops advance through the queue instead of bouncing back to p2/m2.
        let queue = vec![pwm("p1", "m1"), pwm("p2", "m2"), pwm("p3", "m3")];
        let failed = pwm("p1", "m1");
        let tried = vec![pwm("p2", "m2")];
        let providers = vec![
            provider("p1", true, &[], &[]),
            provider("p2", true, &[], &[]),
            provider("p3", true, &[], &[]),
        ];
        let pick = next_failover_model(&queue, &failed, &tried, &providers).expect("should skip tried p2/m2");
        assert_eq!(pick.provider_id, "p3");
        assert_eq!(pick.model, "m3");
    }

    #[test]
    fn exhausts_when_only_remaining_candidate_already_tried() {
        // Queue has p1 (failed) and p2 (already tried) → nothing left → None.
        let queue = vec![pwm("p1", "m1"), pwm("p2", "m2")];
        let failed = pwm("p1", "m1");
        let tried = vec![pwm("p2", "m2")];
        let providers = vec![provider("p1", true, &[], &[]), provider("p2", true, &[], &[])];
        assert!(next_failover_model(&queue, &failed, &tried, &providers).is_none());
    }

    #[test]
    fn skips_disabled_provider() {
        let queue = vec![pwm("p1", "m1"), pwm("p2", "m2")];
        let failed = pwm("orig", "orig");
        // p1 是禁用 provider → 跳过,落到 p2。
        let providers = vec![provider("p1", false, &[], &[]), provider("p2", true, &[], &[])];
        let pick = next_failover_model(&queue, &failed, &[], &providers).expect("should skip disabled p1");
        assert_eq!(pick.provider_id, "p2");
    }

    #[test]
    fn skips_model_disabled() {
        let queue = vec![pwm("p1", "m1"), pwm("p1", "m2")];
        let failed = pwm("orig", "orig");
        // p1 的 m1 被显式禁用 → 跳过,落到 m2。
        let providers = vec![provider("p1", true, &[("m1", false), ("m2", true)], &[])];
        let pick = next_failover_model(&queue, &failed, &[], &providers).expect("should skip disabled m1");
        assert_eq!(pick.model, "m2");
    }

    #[test]
    fn skips_unhealthy_model() {
        let queue = vec![pwm("p1", "m1"), pwm("p1", "m2")];
        let failed = pwm("orig", "orig");
        // m1 标 Unhealthy → 跳过;m2 Healthy → 选中。
        let providers = vec![provider(
            "p1",
            true,
            &[],
            &[("m1", HealthStatus::Unhealthy), ("m2", HealthStatus::Healthy)],
        )];
        let pick = next_failover_model(&queue, &failed, &[], &providers).expect("should skip unhealthy m1");
        assert_eq!(pick.model, "m2");
    }

    #[test]
    fn unknown_health_is_still_a_candidate() {
        // Unknown / 无健康记录 不应被跳过(只有 Unhealthy 才排除)。
        let queue = vec![pwm("p1", "m1")];
        let failed = pwm("orig", "orig");
        let providers = vec![provider("p1", true, &[], &[("m1", HealthStatus::Unknown)])];
        assert!(next_failover_model(&queue, &failed, &[], &providers).is_some());
    }

    #[test]
    fn returns_none_when_exhausted() {
        // 队列里唯一候选就是刚失败的那个 → 耗尽 → None。
        let queue = vec![pwm("p1", "m1")];
        let failed = pwm("p1", "m1");
        let providers = vec![provider("p1", true, &[], &[])];
        assert!(next_failover_model(&queue, &failed, &[], &providers).is_none());
    }

    #[test]
    fn returns_none_when_all_unavailable() {
        let queue = vec![pwm("p1", "m1"), pwm("p2", "m2")];
        let failed = pwm("orig", "orig");
        // p1 禁用 + p2 的 m2 Unhealthy → 全不可用 → None。
        let providers = vec![
            provider("p1", false, &[], &[]),
            provider("p2", true, &[], &[("m2", HealthStatus::Unhealthy)]),
        ];
        assert!(next_failover_model(&queue, &failed, &[], &providers).is_none());
    }

    #[test]
    fn missing_provider_row_is_not_a_candidate() {
        // 候选引用的 provider 不在表中 → 找不到即不可用,跳到下一个。
        let queue = vec![pwm("ghost", "m1"), pwm("p2", "m2")];
        let failed = pwm("orig", "orig");
        let providers = vec![provider("p2", true, &[], &[])];
        let pick = next_failover_model(&queue, &failed, &[], &providers).expect("should fall to p2");
        assert_eq!(pick.provider_id, "p2");
    }

    #[test]
    fn empty_queue_returns_none() {
        let providers = vec![provider("p1", true, &[], &[])];
        assert!(next_failover_model(&[], &pwm("p1", "m1"), &[], &providers).is_none());
    }

    #[test]
    fn malformed_model_health_json_fails_open() {
        // model_health 是垃圾字符串 → 按未知健康处理,候选仍可用。
        let mut p = provider("p1", true, &[], &[]);
        p.model_health = Some("{not json".into());
        let queue = vec![pwm("p1", "m1")];
        assert!(next_failover_model(&queue, &pwm("orig", "orig"), &[], &[p]).is_some());
    }

    // ── 配置读写 ──

    #[test]
    fn conversation_override_present_parses() {
        let extra = serde_json::json!({
            "workspace": "/tmp/x",
            "model_failover": {"enabled": true, "max_switches": 2}
        })
        .to_string();
        let cfg = read_conversation_failover_override(&extra).expect("override present");
        assert!(cfg.enabled);
        assert_eq!(cfg.max_switches, 2);
        // 未给的字段仍走默认。
        assert!(cfg.stamp_unhealthy);
    }

    #[test]
    fn conversation_override_absent_is_none() {
        let extra = serde_json::json!({"workspace": "/tmp/x"}).to_string();
        assert!(read_conversation_failover_override(&extra).is_none());
    }

    #[test]
    fn conversation_override_malformed_extra_is_none() {
        assert!(read_conversation_failover_override("{not json").is_none());
    }

    // ── 故障分类(本地副本与 idmm 表对齐)──

    #[test]
    fn is_provider_fault_matches_known_codes() {
        assert!(is_provider_fault(AgentErrorCode::UserLlmProviderRateLimited));
        assert!(is_provider_fault(AgentErrorCode::UserLlmProviderGatewayError));
        assert!(is_provider_fault(AgentErrorCode::UserLlmProviderTimeout));
        assert!(is_provider_fault(AgentErrorCode::UnknownUpstreamError));
        // 非 provider 故障:用户取消 / 会话忙 等不应触发转移。
        assert!(!is_provider_fault(AgentErrorCode::UserAgentNotInstalled));
        assert!(!is_provider_fault(AgentErrorCode::NomifunConversationBusy));
    }

    #[test]
    fn image_unsupported_is_not_provider_fault() {
        assert!(!is_provider_fault(AgentErrorCode::UserLlmProviderImageUnsupported));
    }
}

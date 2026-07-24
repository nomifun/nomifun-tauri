//! Public DTOs for IDMM (Intelligent Decision-Making Mode) — a per-session,
//! opt-in supervision capability that keeps agent/terminal sessions alive
//! through provider faults and decision stalls. Pure serde — no axum.
//!
//! Phase 2 重组为两个可独立开关、默认关的「值守」(spec §5.1/5.2):故障值守
//! (`fault_watch`)与决策值守(`decision_watch`),各持一套 [`WatchBase`] 旋钮 +
//! 旁路模型;决策值守额外带结构化决策策略([`DecisionStrategy`])与纯问答开关。

use nomifun_common::IdmmInterventionId;
use serde::{Deserialize, Serialize};

/// Which kind of session IDMM supervises.
/// conversation ID can never be interpreted as a terminal ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdmmTargetKind {
    Conversation,
    Terminal,
}

impl IdmmTargetKind {
    /// Parse the path-segment form (`conversation` | `terminal`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "conversation" => Some(Self::Conversation),
            "terminal" => Some(Self::Terminal),
            _ => None,
        }
    }

    /// The persisted / path-segment string form (`conversation` | `terminal`).
    /// Inverse of [`Self::parse`]; the single source for the `target_kind`
    /// column + path segment, reused by the IDMM service + supervisor.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Terminal => "terminal",
        }
    }
}

/// 单个值守的处理档位(取代 Phase-1 的 `IdmmTier`)。
///
/// - `RuleOnly` = 无模型规则检查模式:只跑规则,不升级旁路模型。
/// - `RulePlusModel` = 旁路模型混合:规则扫描日志 + 模型做决策(可升级)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WatchTier {
    /// 无模型规则检查模式。
    #[default]
    RuleOnly,
    /// 规则 + 旁路模型混合。
    RulePlusModel,
}

/// 扫描内容范围:送给规则/旁路模型的会话上下文裁剪口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScanScope {
    /// 仅末轮(默认,等价 Phase-1 行为)。
    #[default]
    LastTurn,
    /// 最近 N 条(N 由 `max_context_chars` 同侧的预算约束;条数语义在引擎层取用)。
    LastMessages,
    /// 全会话。
    FullSession,
}

/// 唤醒动作策略(故障值守)。`P2` 仅 `Retry` 生效;`Failover` 系列在 P3 接故障
/// 转移队列后启用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WakeStrategy {
    /// 仅重试 / 唤醒原会话(P2 默认且唯一生效)。
    #[default]
    Retry,
    /// 故障转移到队列中下一个可用模型(P3)。
    Failover,
    /// 先故障转移,失败再重试(P3)。
    FailoverThenRetry,
}

/// 决策倾向:影响规则档无推荐项时是否敢挑、纯问答果断程度、Idle 倾向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Tendency {
    /// 保守:倾向 Halt。
    Conservative,
    /// 均衡(默认)。
    #[default]
    Balanced,
    /// 激进:更敢自动推进。
    Aggressive,
}

/// 被阻塞(无安全自动解)时的行为倾向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BlockedBehavior {
    /// 倾向继续(默认)。
    #[default]
    PreferContinue,
    /// 倾向暂停。
    PreferPause,
    /// 必须问人。
    MustAsk,
}

/// 分类规则的处理模式。`Auto` = 自动处理;`AskFirst`/`Off` 本期均退回人类
/// (Halt),不引入异步问人通道(详见 plan D5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CategoryMode {
    /// 按规则/模型自动处理(默认)。
    #[default]
    Auto,
    /// 先问人(本期等同 Off:Halt)。
    AskFirst,
    /// 关闭该分类的自动处理(Halt)。
    Off,
}

fn default_scan_interval_secs() -> u32 {
    60
}
fn default_max_retries() -> u32 {
    5
}
fn default_true() -> bool {
    true
}
fn default_max_context_chars() -> u32 {
    8000
}
fn default_open_answer_chars() -> u32 {
    600
}
fn default_max_interventions_per_hour() -> u32 {
    30
}
fn default_min_interval_secs() -> u32 {
    20
}

/// Rate limits to keep IDMM from thrashing a session. 每个值守各持一份(预算/
/// 最小间隔按值守各自计,plan D4)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    #[serde(default = "default_max_interventions_per_hour")]
    pub max_interventions_per_hour: u32,
    #[serde(default = "default_min_interval_secs")]
    pub min_interval_secs: u32,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_interventions_per_hour: default_max_interventions_per_hour(),
            min_interval_secs: default_min_interval_secs(),
        }
    }
}

/// 旁路模型供应商选择。空 → 全局默认(`idmm_backup_*`)→ 会话自身模型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct BypassModelRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl<'de> Deserialize<'de> for BypassModelRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(default)]
            provider_id: Option<String>,
            #[serde(default)]
            model: Option<String>,
        }

        let wire = Wire::deserialize(deserializer)?;
        crate::serde_util::validate_optional_provider_model_pair(
            wire.provider_id.as_deref(),
            wire.model.as_deref(),
        )
        .map_err(serde::de::Error::custom)?;
        Ok(Self {
            provider_id: wire.provider_id,
            model: wire.model,
        })
    }
}

/// 两个值守共享的基础旋钮(spec §5.1)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchBase {
    /// 默认 false —— 值守 opt-in。
    #[serde(default)]
    pub enabled: bool,
    /// 处理档位:`RuleOnly`(无模型)| `RulePlusModel`(旁路模型混合)。
    #[serde(default)]
    pub tier: WatchTier,
    /// 监测间隔(=Phase-1 `idle_threshold_secs`,默认 60 秒)。
    #[serde(default = "default_scan_interval_secs")]
    pub scan_interval_secs: u32,
    /// 最大重试次数(默认 5)。
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// 扫描内容范围。
    #[serde(default)]
    pub scan_scope: ScanScope,
    /// 送给旁路模型的上下文字符上限(沿用 Phase-1 默认 8000)。
    #[serde(default = "default_max_context_chars")]
    pub max_context_chars: u32,
    /// 旁路模型供应商选择(空 → 全局默认 → 会话自身模型)。
    #[serde(default)]
    pub bypass_model: BypassModelRef,
    /// 预算/最小间隔(复用 Phase-1 `BudgetConfig`)。
    #[serde(default)]
    pub budget: BudgetConfig,
}

impl Default for WatchBase {
    fn default() -> Self {
        Self {
            enabled: false,
            tier: WatchTier::RuleOnly,
            scan_interval_secs: default_scan_interval_secs(),
            max_retries: default_max_retries(),
            scan_scope: ScanScope::LastTurn,
            max_context_chars: default_max_context_chars(),
            bypass_model: BypassModelRef::default(),
            budget: BudgetConfig::default(),
        }
    }
}

/// 故障值守:检测供应商故障 / 网络异常 → 自动唤醒、重试 / 故障转移。
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FaultWatchConfig {
    #[serde(flatten)]
    pub base: WatchBase,
    /// 唤醒动作(P2 仅 `Retry` 生效)。
    #[serde(default)]
    pub wake_action: WakeStrategy,
    /// 是否调用模型故障转移队列(P3 用,P2 占位 false)。
    #[serde(default)]
    pub use_failover_queue: bool,
}

impl<'de> Deserialize<'de> for FaultWatchConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(default)]
            enabled: bool,
            #[serde(default)]
            tier: WatchTier,
            #[serde(default = "default_scan_interval_secs")]
            scan_interval_secs: u32,
            #[serde(default = "default_max_retries")]
            max_retries: u32,
            #[serde(default)]
            scan_scope: ScanScope,
            #[serde(default = "default_max_context_chars")]
            max_context_chars: u32,
            #[serde(default)]
            bypass_model: BypassModelRef,
            #[serde(default)]
            budget: BudgetConfig,
            #[serde(default)]
            wake_action: WakeStrategy,
            #[serde(default)]
            use_failover_queue: bool,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            base: WatchBase {
                enabled: wire.enabled,
                tier: wire.tier,
                scan_interval_secs: wire.scan_interval_secs,
                max_retries: wire.max_retries,
                scan_scope: wire.scan_scope,
                max_context_chars: wire.max_context_chars,
                bypass_model: wire.bypass_model,
                budget: wire.budget,
            },
            wake_action: wire.wake_action,
            use_failover_queue: wire.use_failover_queue,
        })
    }
}

/// 选项决策规则。默认值等价 Phase-1 `RuleConfig`(plan D5):
/// `prefer_recommended` ↔ 旧 `auto_accept_recommended`;`allow_unmarked_pick` ↔
/// 旧 `auto_pick_unmarked`;`never_destructive` ↔ 旧 `!allow_destructive`(取反,
/// 默认 true = 不碰破坏性)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionRule {
    #[serde(default)]
    pub mode: CategoryMode,
    #[serde(default = "default_true")]
    pub prefer_recommended: bool,
    #[serde(default = "default_true")]
    pub allow_unmarked_pick: bool,
    #[serde(default = "default_true")]
    pub never_destructive: bool,
}

impl Default for OptionRule {
    fn default() -> Self {
        Self {
            mode: CategoryMode::Auto,
            prefer_recommended: true,
            allow_unmarked_pick: true,
            never_destructive: true,
        }
    }
}

/// 纯问答规则。`max_answer_chars` 默认 600(plan D2)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenQuestionRule {
    #[serde(default)]
    pub mode: CategoryMode,
    #[serde(default = "default_open_answer_chars")]
    pub max_answer_chars: u32,
}

impl Default for OpenQuestionRule {
    fn default() -> Self {
        Self {
            mode: CategoryMode::Auto,
            max_answer_chars: default_open_answer_chars(),
        }
    }
}

/// 权限确认规则。`only_safe_value` + `escalate_risky` 对应 Phase-1 安全闸
/// (只读工具自动 confirm,风险升级)——**安全闸不可破**(plan D5,默认 true)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRule {
    #[serde(default)]
    pub mode: CategoryMode,
    #[serde(default = "default_true")]
    pub only_safe_value: bool,
    #[serde(default = "default_true")]
    pub escalate_risky: bool,
}

impl Default for PermissionRule {
    fn default() -> Self {
        Self {
            mode: CategoryMode::Auto,
            only_safe_value: true,
            escalate_risky: true,
        }
    }
}

/// 三类分类规则(spec §5.2)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CategoryRules {
    #[serde(default)]
    pub option_decision: OptionRule,
    #[serde(default)]
    pub open_question: OpenQuestionRule,
    #[serde(default)]
    pub permission: PermissionRule,
}

/// 结构化决策策略(spec §5.2):倾向 + 阻塞行为 + 三类分类规则 + 自由文本兜底。
/// 结构化字段驱动规则档、约束模型档;`freeform_policy` 仅在模型档拼进 sidecar
/// 提示词(prompt.rs),不参与规则档。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DecisionStrategy {
    #[serde(default)]
    pub tendency: Tendency,
    #[serde(default)]
    pub on_blocked: BlockedBehavior,
    #[serde(default)]
    pub categories: CategoryRules,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freeform_policy: Option<String>,
}

/// 决策值守:检测会话内问题(选项决策 + 纯问答)→ 自动作答。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionWatchConfig {
    #[serde(flatten)]
    pub base: WatchBase,
    #[serde(default)]
    pub strategy: DecisionStrategy,
    /// 纯问答开关。**旁路模型档默认开启**(选模型档即意味着让模型替你决策,
    /// 含开放式提问);可手动关闭。在规则档惰性无影响(规则档永不作答纯问答)。
    #[serde(default = "default_true")]
    pub answer_open_questions: bool,
}

impl<'de> Deserialize<'de> for DecisionWatchConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(default)]
            enabled: bool,
            #[serde(default)]
            tier: WatchTier,
            #[serde(default = "default_scan_interval_secs")]
            scan_interval_secs: u32,
            #[serde(default = "default_max_retries")]
            max_retries: u32,
            #[serde(default)]
            scan_scope: ScanScope,
            #[serde(default = "default_max_context_chars")]
            max_context_chars: u32,
            #[serde(default)]
            bypass_model: BypassModelRef,
            #[serde(default)]
            budget: BudgetConfig,
            #[serde(default)]
            strategy: DecisionStrategy,
            #[serde(default = "default_true")]
            answer_open_questions: bool,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            base: WatchBase {
                enabled: wire.enabled,
                tier: wire.tier,
                scan_interval_secs: wire.scan_interval_secs,
                max_retries: wire.max_retries,
                scan_scope: wire.scan_scope,
                max_context_chars: wire.max_context_chars,
                bypass_model: wire.bypass_model,
                budget: wire.budget,
            },
            strategy: wire.strategy,
            answer_open_questions: wire.answer_open_questions,
        })
    }
}
impl Default for DecisionWatchConfig {
    fn default() -> Self {
        Self {
            base: WatchBase::default(),
            strategy: DecisionStrategy::default(),
            answer_open_questions: true,
        }
    }
}

/// The full per-session IDMM config (persisted as JSON in
/// `conversation.extra.idmm` or `terminal_sessions.idmm`).
///
/// v3 accepts only this two-watch structure. Unknown or retired fields are
/// rejected rather than interpreted as defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct IdmmConfig {
    /// 故障值守。
    #[serde(default)]
    pub fault_watch: FaultWatchConfig,
    /// 决策值守。
    #[serde(default)]
    pub decision_watch: DecisionWatchConfig,
}

impl IdmmConfig {
    /// 「启用」当且仅当任一值守开启(plan D4)。`IdmmManager::ensure` / service 的
    /// enable 判断改读此。
    pub fn any_enabled(&self) -> bool {
        self.fault_watch.base.enabled || self.decision_watch.base.enabled
    }
}

/// Tri-state run state for the UI dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdmmRunState {
    /// Disabled.
    Off,
    /// Enabled, supervising, no active intervention.
    Armed,
    /// Mid-intervention right now.
    Intervening,
}

/// Live state surfaced to the client + the `idmm.statusChanged` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdmmState {
    pub kind: IdmmTargetKind,
    #[serde(deserialize_with = "crate::serde_util::deserialize_session_target_id")]
    pub target_id: String,
    /// 任一值守启用即 true(plan D4:`enabled` 表示「任一值守开」)。
    pub enabled: bool,
    /// 故障值守是否启用(per-watch 运行概要,廉价补充)。
    #[serde(default)]
    pub fault_enabled: bool,
    /// 决策值守是否启用(per-watch 运行概要)。
    #[serde(default)]
    pub decision_enabled: bool,
    pub run_state: IdmmRunState,
    pub interventions_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_signal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_intervention_at: Option<i64>,
    /// Whether a backup provider is resolvable (per-session or global default).
    pub sidecar_provider_resolved: bool,
    /// The persisted per-session `IdmmConfig`, included so the frontend can
    /// rehydrate its form (per-watch tier, bypass model, strategy rules) on
    /// remount instead of reconstructing from scratch and silently dropping the
    /// user's saved config. Absent for targets that have never been configured
    /// (the frontend then falls back to the global defaults via `IdmmSettings`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<IdmmConfig>,
}

impl IdmmState {
    /// Compute the tri-state run state from enabled + intervening flags.
    pub fn run_state(enabled: bool, intervening: bool) -> IdmmRunState {
        if !enabled {
            IdmmRunState::Off
        } else if intervening {
            IdmmRunState::Intervening
        } else {
            IdmmRunState::Armed
        }
    }
}

/// One row of the intervention audit log + the `idmm.intervention` payload.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InterventionRecord {
    /// Stable bare UUIDv7 audit-record business identity.
    pub intervention_id: IdmmInterventionId,
    /// `"conversation"` or `"terminal"`.
    pub target_kind: String,
    #[serde(deserialize_with = "crate::serde_util::deserialize_session_target_id")]
    pub target_id: String,
    /// "fault" | "decision".
    pub watch: String,
    pub at: i64,
    /// "provider_error" | "idle" | "decision" | "scheduled".
    pub stall_class: String,
    /// "rule" | "sidecar" | "rule_fallback".
    pub tier_used: String,
    /// "option" | "open_question" | "permission" | "fault"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// "retry" | "send_text" | "answer_choice" | "wait" | "stop".
    pub action: String,
    /// 选了什么/答了什么(截断 ≤2000 字符)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Canonical disposition token the UI renders/colors. One of
    /// `applied | resolved | failed | halted | skipped` — Phase-1 emits
    /// `applied` (an action was injected) or `halted` (stood down, needs a
    /// human). The free-form *why* lives in `reason`, never here.
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 模型置信度(规则档 None)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// provider/model(规则档 None)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bypass_model: Option<String>,
}

impl<'de> Deserialize<'de> for InterventionRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            intervention_id: IdmmInterventionId,
            target_kind: String,
            #[serde(deserialize_with = "crate::serde_util::deserialize_session_target_id")]
            target_id: String,
            watch: String,
            at: i64,
            stall_class: String,
            tier_used: String,
            #[serde(default)]
            category: Option<String>,
            action: String,
            #[serde(default)]
            detail: Option<String>,
            outcome: String,
            #[serde(default)]
            reason: Option<String>,
            #[serde(default)]
            confidence: Option<f32>,
            #[serde(default)]
            bypass_model: Option<String>,
        }

        let wire = Wire::deserialize(deserializer)?;
        if IdmmTargetKind::parse(&wire.target_kind).is_none() {
            return Err(serde::de::Error::custom(
                "target_kind must be conversation or terminal",
            ));
        }
        Ok(Self {
            intervention_id: wire.intervention_id,
            target_kind: wire.target_kind,
            target_id: wire.target_id,
            watch: wire.watch,
            at: wire.at,
            stall_class: wire.stall_class,
            tier_used: wire.tier_used,
            category: wire.category,
            action: wire.action,
            detail: wire.detail,
            outcome: wire.outcome,
            reason: wire.reason,
            confidence: wire.confidence,
            bypass_model: wire.bypass_model,
        })
    }
}

/// Request body for `POST /api/idmm`.
#[derive(Debug, Clone, Serialize)]
pub struct SetIdmmRequest {
    pub kind: IdmmTargetKind,
    /// Canonical conversation or terminal entity ID. JSON numbers are rejected.
    #[serde(deserialize_with = "crate::serde_util::deserialize_session_target_id")]
    pub target_id: String,
    #[serde(flatten)]
    pub config: IdmmConfig,
}

impl<'de> Deserialize<'de> for SetIdmmRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            kind: IdmmTargetKind,
            #[serde(deserialize_with = "crate::serde_util::deserialize_session_target_id")]
            target_id: String,
            #[serde(default)]
            fault_watch: FaultWatchConfig,
            #[serde(default)]
            decision_watch: DecisionWatchConfig,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            kind: wire.kind,
            target_id: wire.target_id,
            config: IdmmConfig {
                fault_watch: wire.fault_watch,
                decision_watch: wire.decision_watch,
            },
        })
    }
}

/// Global IDMM defaults (`GET/PUT /api/idmm/settings`), stored in `client_preferences`.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct IdmmSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_model: Option<String>,
    #[serde(default)]
    pub default_steering_prompt: String,
}

impl<'de> Deserialize<'de> for IdmmSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(default)]
            backup_provider_id: Option<String>,
            #[serde(default)]
            backup_model: Option<String>,
            #[serde(default)]
            default_steering_prompt: String,
        }

        let wire = Wire::deserialize(deserializer)?;
        crate::serde_util::validate_optional_provider_model_pair(
            wire.backup_provider_id.as_deref(),
            wire.backup_model.as_deref(),
        )
        .map_err(serde::de::Error::custom)?;
        Ok(Self {
            backup_provider_id: wire.backup_provider_id,
            backup_model: wire.backup_model,
            default_steering_prompt: wire.default_steering_prompt,
        })
    }
}

fn default_max_switches() -> u32 {
    4
}

/// Phase 3 模型故障转移队列(spec §5.5)。独立于 IDMM 生效,IDMM 故障值守也能触发它。
///
/// 全局存于 `client_preferences` 键 `agent.model_failover`(整体 JSON);会话级
/// 可在 `conversations.extra.model_failover` 覆盖(存在则优先于全局)。所有字段
/// 带 serde 默认,故空对象 → 关闭、空队列、`max_switches=4`、`stamp_unhealthy=true`。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelFailoverConfig {
    /// 默认 false:不配置即不转移。
    #[serde(default)]
    pub enabled: bool,
    /// 有序候选 `(provider_id, model, use_model?)`;按序挑下一个可用模型。
    #[serde(default)]
    pub queue: Vec<nomifun_common::ProviderWithModel>,
    /// 单轮最大切换次数(默认 4;实际还受队列长度封顶)。
    #[serde(default = "default_max_switches")]
    pub max_switches: u32,
    /// 默认 true:故障时把失败模型的 `model_health` 标 `Unhealthy`。
    #[serde(default = "default_true")]
    pub stamp_unhealthy: bool,
}

impl<'de> Deserialize<'de> for ModelFailoverConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(default)]
            enabled: bool,
            #[serde(default)]
            queue: Vec<nomifun_common::ProviderWithModel>,
            #[serde(default = "default_max_switches")]
            max_switches: u32,
            #[serde(default = "default_true")]
            stamp_unhealthy: bool,
        }

        let wire = Wire::deserialize(deserializer)?;
        if let Some(error) = wire.queue.iter().find_map(|entry| entry.validate().err()) {
            return Err(serde::de::Error::custom(format!(
                "invalid model failover queue entry: {error}"
            )));
        }
        Ok(Self {
            enabled: wire.enabled,
            queue: wire.queue,
            max_switches: wire.max_switches,
            stamp_unhealthy: wire.stamp_unhealthy,
        })
    }
}

impl Default for ModelFailoverConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            queue: Vec::new(),
            max_switches: default_max_switches(),
            stamp_unhealthy: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认空对象 → 两个值守皆关,且各旋钮回到 Phase-1 等价默认(D5)。
    #[test]
    fn idmm_config_defaults_deserialize_from_empty_object() {
        let cfg: IdmmConfig = serde_json::from_str("{}").unwrap();
        assert!(!cfg.any_enabled());
        // 故障值守
        assert!(!cfg.fault_watch.base.enabled);
        assert_eq!(cfg.fault_watch.base.tier, WatchTier::RuleOnly);
        assert_eq!(cfg.fault_watch.base.scan_interval_secs, 60);
        assert_eq!(cfg.fault_watch.base.max_retries, 5);
        assert_eq!(cfg.fault_watch.base.scan_scope, ScanScope::LastTurn);
        assert_eq!(cfg.fault_watch.base.max_context_chars, 8000);
        assert!(cfg.fault_watch.base.bypass_model.provider_id.is_none());
        assert_eq!(cfg.fault_watch.base.budget.max_interventions_per_hour, 30);
        assert_eq!(cfg.fault_watch.base.budget.min_interval_secs, 20);
        assert_eq!(cfg.fault_watch.wake_action, WakeStrategy::Retry);
        assert!(!cfg.fault_watch.use_failover_queue);
        // 决策值守
        assert!(!cfg.decision_watch.base.enabled);
        assert_eq!(cfg.decision_watch.base.tier, WatchTier::RuleOnly);
        // 纯问答默认开(旁路模型档生效;规则档惰性无影响)。
        assert!(cfg.decision_watch.answer_open_questions);
    }

    /// D5:决策策略默认值必须等价 Phase-1 行为(开箱即用且安全)。
    #[test]
    fn decision_strategy_defaults_match_phase1_behavior() {
        let s = DecisionStrategy::default();
        assert_eq!(s.tendency, Tendency::Balanced);
        assert_eq!(s.on_blocked, BlockedBehavior::PreferContinue);
        assert!(s.freeform_policy.is_none());
        // 选项决策:prefer_recommended ↔ auto_accept_recommended、
        // allow_unmarked_pick ↔ auto_pick_unmarked、never_destructive ↔ !allow_destructive
        let opt = &s.categories.option_decision;
        assert_eq!(opt.mode, CategoryMode::Auto);
        assert!(opt.prefer_recommended);
        assert!(opt.allow_unmarked_pick);
        assert!(opt.never_destructive);
        // 权限:只读放行 + 风险升级(安全闸不可破)
        let perm = &s.categories.permission;
        assert_eq!(perm.mode, CategoryMode::Auto);
        assert!(perm.only_safe_value);
        assert!(perm.escalate_risky);
        // 纯问答默认 600 字符上限
        assert_eq!(s.categories.open_question.mode, CategoryMode::Auto);
        assert_eq!(s.categories.open_question.max_answer_chars, 600);
    }

    /// Retired Phase-1 fields are not part of the v3 shape.
    #[test]
    fn retired_idmm_config_shape_is_rejected() {
        let retired = serde_json::json!({
            "enabled": true,
            "tier": "rule_plus_sidecar",
            "steering_prompt": "prefer the recommended option; never delete data",
            "rule": {
                "idle_threshold_secs": 90,
                "auto_retry": true,
                "max_retries": 9,
                "auto_accept_recommended": true,
                "auto_pick_unmarked": false,
                "allow_destructive": true
            },
            "sidecar": {
                "provider_id": "openrouter",
                "model": "gpt-x",
                "read_history": true,
                "session_mode": "continuous",
                "max_context_chars": 12000,
                "confidence_floor": 0.4,
                "scheduled_check": {"enabled": true, "every_secs": 300}
            },
            "budget": {"max_interventions_per_hour": 99, "min_interval_secs": 1}
        });
        assert!(serde_json::from_value::<IdmmConfig>(retired).is_err());
    }

    /// 新形态往返:配置 → JSON → 配置 保真。
    #[test]
    fn new_shape_roundtrip() {
        const PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
        let cfg = IdmmConfig {
            fault_watch: FaultWatchConfig {
                base: WatchBase {
                    enabled: true,
                    tier: WatchTier::RulePlusModel,
                    scan_interval_secs: 45,
                    max_retries: 3,
                    scan_scope: ScanScope::FullSession,
                    max_context_chars: 5000,
                    bypass_model: BypassModelRef {
                        provider_id: Some(PROVIDER_ID.into()),
                        model: Some("gpt-x".into()),
                    },
                    budget: BudgetConfig {
                        max_interventions_per_hour: 10,
                        min_interval_secs: 5,
                    },
                },
                wake_action: WakeStrategy::FailoverThenRetry,
                use_failover_queue: true,
            },
            decision_watch: DecisionWatchConfig {
                base: WatchBase {
                    enabled: true,
                    tier: WatchTier::RulePlusModel,
                    ..WatchBase::default()
                },
                strategy: DecisionStrategy {
                    tendency: Tendency::Aggressive,
                    on_blocked: BlockedBehavior::MustAsk,
                    categories: CategoryRules {
                        option_decision: OptionRule {
                            mode: CategoryMode::Auto,
                            prefer_recommended: false,
                            allow_unmarked_pick: false,
                            never_destructive: true,
                        },
                        open_question: OpenQuestionRule {
                            mode: CategoryMode::Off,
                            max_answer_chars: 1200,
                        },
                        permission: PermissionRule::default(),
                    },
                    freeform_policy: Some("stay on task, never touch prod".into()),
                },
                answer_open_questions: true,
            },
        };
        let json = serde_json::to_value(&cfg).unwrap();
        // flatten 把 base 字段提到 fault_watch / decision_watch 顶层。
        assert_eq!(json["fault_watch"]["enabled"], serde_json::json!(true));
        assert_eq!(json["fault_watch"]["tier"], serde_json::json!("rule_plus_model"));
        assert_eq!(json["fault_watch"]["wake_action"], serde_json::json!("failover_then_retry"));
        assert_eq!(json["decision_watch"]["answer_open_questions"], serde_json::json!(true));
        assert_eq!(json["decision_watch"]["strategy"]["tendency"], serde_json::json!("aggressive"));
        let back: IdmmConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn bypass_model_requires_a_complete_trimmed_provider_model_pair() {
        const PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
        for invalid in [
            serde_json::json!({"provider_id": PROVIDER_ID}),
            serde_json::json!({"model": "model-a"}),
            serde_json::json!({"provider_id": "openrouter", "model": "model-a"}),
            serde_json::json!({"provider_id": PROVIDER_ID, "model": ""}),
            serde_json::json!({"provider_id": PROVIDER_ID, "model": " model-a"}),
            serde_json::json!({"provider_id": PROVIDER_ID, "model": "model-a "}),
        ] {
            assert!(serde_json::from_value::<BypassModelRef>(invalid).is_err());
        }
        assert!(
            serde_json::from_value::<BypassModelRef>(serde_json::json!({
                "provider_id": PROVIDER_ID,
                "model": "model-a"
            }))
            .is_ok()
        );
    }

    #[test]
    fn idmm_settings_require_a_complete_trimmed_provider_model_pair() {
        const PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
        for invalid in [
            serde_json::json!({"backup_provider_id": PROVIDER_ID}),
            serde_json::json!({"backup_model": "model-a"}),
            serde_json::json!({
                "backup_provider_id": "openrouter",
                "backup_model": "model-a"
            }),
            serde_json::json!({
                "backup_provider_id": PROVIDER_ID,
                "backup_model": " model-a"
            }),
        ] {
            assert!(serde_json::from_value::<IdmmSettings>(invalid).is_err());
        }
        assert!(
            serde_json::from_value::<IdmmSettings>(serde_json::json!({
                "backup_provider_id": PROVIDER_ID,
                "backup_model": "model-a"
            }))
            .is_ok()
        );
    }

    /// flatten 让 `SetIdmmRequest` 与值守配置在同一对象里共存(真实 payload 形态)。
    #[test]
    fn set_idmm_request_flattens_config() {
        let target_id = "0190f5fe-7c00-7a00-8000-000000000001";
        let json = serde_json::json!({
            "kind": "terminal",
            "target_id": target_id,
            "fault_watch": {"enabled": true, "tier": "rule_plus_model"},
            "decision_watch": {"enabled": true, "answer_open_questions": true}
        });
        let req: SetIdmmRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.kind, IdmmTargetKind::Terminal);
        assert_eq!(req.target_id, target_id);
        assert!(req.config.any_enabled());
        assert!(req.config.fault_watch.base.enabled);
        assert_eq!(req.config.fault_watch.base.tier, WatchTier::RulePlusModel);
        assert!(req.config.decision_watch.answer_open_questions);
    }

    #[test]
    fn idmm_run_state_tristate() {
        assert_eq!(IdmmState::run_state(false, false), IdmmRunState::Off);
        assert_eq!(IdmmState::run_state(false, true), IdmmRunState::Off);
        assert_eq!(IdmmState::run_state(true, false), IdmmRunState::Armed);
        assert_eq!(IdmmState::run_state(true, true), IdmmRunState::Intervening);
    }

    #[test]
    fn watch_tier_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(WatchTier::RulePlusModel).unwrap(),
            serde_json::json!("rule_plus_model")
        );
        assert_eq!(serde_json::to_value(WatchTier::RuleOnly).unwrap(), serde_json::json!("rule_only"));
    }

    #[test]
    fn target_kind_and_run_state_serialize_snake_case() {
        assert_eq!(
            serde_json::to_value(IdmmTargetKind::Conversation).unwrap(),
            serde_json::json!("conversation")
        );
        assert_eq!(
            serde_json::to_value(IdmmRunState::Intervening).unwrap(),
            serde_json::json!("intervening")
        );
    }

    /// v3 accepts only a canonical entity string; numeric legacy IDs are
    /// rejected instead of guessed or coerced.
    #[test]
    fn set_idmm_request_rejects_numeric_target_id() {
        let body = r#"{"kind":"conversation","target_id":2,"fault_watch":{"enabled":true}}"#;
        assert!(serde_json::from_str::<SetIdmmRequest>(body).is_err());
    }

    #[test]
    fn set_idmm_request_rejects_numeric_target_id_with_flattened_config() {
        let json = serde_json::json!({
            "kind": "conversation",
            "target_id": 12345,
            "decision_watch": {"enabled": true, "strategy": {"tendency": "conservative"}}
        });
        assert!(serde_json::from_value::<SetIdmmRequest>(json).is_err());
    }

    #[test]
    fn set_idmm_request_accepts_string_target_id() {
        let body = r#"{"kind":"terminal","target_id":"0190f5fe-7c00-7a00-8000-000000000007"}"#;
        let req: SetIdmmRequest =
            serde_json::from_str(body).expect("string target_id must deserialize");
        assert_eq!(
            req.target_id,
            "0190f5fe-7c00-7a00-8000-000000000007"
        );
        assert_eq!(req.kind, IdmmTargetKind::Terminal);
        assert!(!req.config.any_enabled());
    }

    #[test]
    fn set_idmm_request_rejects_non_canonical_entity_string() {
        let body = r#"{"kind":"terminal","target_id":"term_7"}"#;
        assert!(serde_json::from_str::<SetIdmmRequest>(body).is_err());
    }

    #[test]
    fn set_idmm_request_rejects_unknown_top_level_field() {
        let body = r#"{
            "kind":"terminal",
            "target_id":"0190f5fe-7c00-7a00-8000-000000000007",
            "enabled":true
        }"#;
        assert!(serde_json::from_str::<SetIdmmRequest>(body).is_err());
    }

    #[test]
    fn set_idmm_request_rejects_unknown_nested_fields() {
        let target_id = "0190f5fe-7c00-7a00-8000-000000000001";
        for value in [
            serde_json::json!({
                "kind": "conversation",
                "target_id": target_id,
                "fault_watch": { "legacy_field": true }
            }),
            serde_json::json!({
                "kind": "conversation",
                "target_id": target_id,
                "decision_watch": {
                    "strategy": { "legacy_field": true }
                }
            }),
            serde_json::json!({
                "kind": "conversation",
                "target_id": target_id,
                "decision_watch": {
                    "budget": { "legacy_field": true }
                }
            }),
        ] {
            assert!(serde_json::from_value::<SetIdmmRequest>(value).is_err());
        }
    }

    #[test]
    fn intervention_record_enriched_fields_roundtrip() {
        let intervention_id = IdmmInterventionId::new();
        let r = InterventionRecord {
            intervention_id: intervention_id.clone(),
            target_kind: "conversation".into(),
            target_id: "0190f5fe-7c00-7a00-8000-000000000001".into(),
            watch: "decision".into(),
            at: 1,
            stall_class: "decision".into(),
            tier_used: "sidecar".into(),
            category: Some("open_question".into()),
            action: "send_text".into(),
            detail: Some("用方案B".into()),
            outcome: "applied".into(),
            reason: Some("因为…".into()),
            confidence: Some(0.82),
            bypass_model: Some("prov:gpt-x".into()),
        };
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["intervention_id"], intervention_id.as_str());
        assert!(j.get("id").is_none());
        assert_eq!(j["category"], "open_question");
        // confidence 是 f32,序列化为 JSON 后带 f32 精度尾差(0.82 → 0.8199999…),
        // 故按 f32 提升后的 f64 值比对,而非裸字面量。
        assert_eq!(j["confidence"].as_f64().unwrap(), 0.82_f32 as f64);
    }

    #[test]
    fn intervention_record_rejects_legacy_generic_id() {
        let legacy = serde_json::json!({
            "id":42,"target_kind":"terminal","target_id":"0190f5fe-7c00-7a00-8000-000000000001","watch":"fault","at":2,
            "stall_class":"provider_error","tier_used":"rule","action":"retry","outcome":"applied"
        });
        assert!(serde_json::from_value::<InterventionRecord>(legacy).is_err());

        let both = serde_json::json!({
            "id":42,"intervention_id":"0190f5fe-7c00-7a00-8000-000000000008","target_kind":"terminal","target_id":"0190f5fe-7c00-7a00-8000-000000000001","watch":"fault","at":2,
            "stall_class":"provider_error","tier_used":"rule","action":"retry","outcome":"applied"
        });
        assert!(serde_json::from_value::<InterventionRecord>(both).is_err());

        for legacy_id in [
            serde_json::json!(42),
            serde_json::json!("idmm_0190f5fe-7c00-7a00-8000-000000000008"),
            serde_json::json!("0190f5fe-7c00-4a00-8000-000000000008"),
            serde_json::json!("0190F5FE-7C00-7A00-8000-000000000008"),
        ] {
            let value = serde_json::json!({
                "intervention_id":legacy_id,"target_kind":"terminal","target_id":"0190f5fe-7c00-7a00-8000-000000000001","watch":"fault","at":2,
                "stall_class":"provider_error","tier_used":"rule","action":"retry","outcome":"applied"
            });
            assert!(serde_json::from_value::<InterventionRecord>(value).is_err());
        }
    }

    #[test]
    fn idmm_state_omits_optional_none_fields() {
        let st = IdmmState {
            kind: IdmmTargetKind::Conversation,
            target_id: "0190f5fe-7c00-7a00-8000-000000000001".into(),
            enabled: false,
            fault_enabled: false,
            decision_enabled: false,
            run_state: IdmmRunState::Off,
            interventions_count: 0,
            last_signal: None,
            last_intervention_at: None,
            sidecar_provider_resolved: false,
            config: None,
        };
        let json = serde_json::to_value(&st).unwrap();
        assert!(json.get("last_signal").is_none());
        assert!(json.get("last_intervention_at").is_none());
        assert!(json.get("config").is_none());
        assert_eq!(json["run_state"], serde_json::json!("off"));
    }

    /// D1:空对象 → 关闭、空队列、`max_switches=4`、`stamp_unhealthy=true`,
    /// 与 [`ModelFailoverConfig::default`] 等价。
    #[test]
    fn model_failover_config_defaults_from_empty_object() {
        let cfg: ModelFailoverConfig = serde_json::from_str("{}").unwrap();
        assert!(!cfg.enabled);
        assert!(cfg.queue.is_empty());
        assert_eq!(cfg.max_switches, 4);
        assert!(cfg.stamp_unhealthy);
        assert_eq!(cfg, ModelFailoverConfig::default());
    }

    /// 队列与显式旋钮 round-trip;`use_model` 可选项保留。
    #[test]
    fn model_failover_config_roundtrip() {
        let json = serde_json::json!({
            "enabled": true,
            "queue": [
                {"provider_id": "019b0000-0000-7000-8000-000000000001", "model": "gpt-x", "use_model": null},
                {"provider_id": "019b0000-0000-7000-8000-000000000002", "model": "claude", "use_model": "claude-alias"}
            ],
            "max_switches": 2,
            "stamp_unhealthy": false
        });
        let cfg: ModelFailoverConfig = serde_json::from_value(json).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.queue.len(), 2);
        assert_eq!(
            cfg.queue[0].provider_id.as_str(),
            "019b0000-0000-7000-8000-000000000001"
        );
        assert_eq!(cfg.queue[1].use_model.as_deref(), Some("claude-alias"));
        assert_eq!(cfg.max_switches, 2);
        assert!(!cfg.stamp_unhealthy);
    }

    #[test]
    fn model_failover_config_rejects_invalid_model_entries() {
        for invalid in [
            serde_json::json!({
                "queue": [{
                    "provider_id": "019b0000-0000-7000-8000-000000000001",
                    "model": ""
                }]
            }),
            serde_json::json!({
                "queue": [{
                    "provider_id": "019b0000-0000-7000-8000-000000000001",
                    "model": " model-a"
                }]
            }),
            serde_json::json!({
                "queue": [{
                    "provider_id": "019b0000-0000-7000-8000-000000000001",
                    "model": "model-a",
                    "use_model": " "
                }]
            }),
        ] {
            assert!(serde_json::from_value::<ModelFailoverConfig>(invalid).is_err());
        }
    }
}

//! Phase 3 模型故障转移 seam(plan D3/D5/D6)的会话服务侧实现。
//!
//! 纯逻辑(挑选器 / 配置读写 / 故障分类)在 [`crate::model_failover`];本模块只放
//! 需要 `&ConversationService`(仓库 + task_manager)的有副作用步骤,并把
//! 「挑下一候选 → 写 `conversation.model` →(可选)标失败模型 Unhealthy →
//! kill_and_wait → 重建任务」抽成**一个** pub 方法 [`ConversationService::perform_model_failover`],
//! 供 send-loop(D3)与 IDMM 故障值守(D6,Task 3)共用同一份实现。
//!
//! 这是 [`crate::acp_error_recovery::ConversationService::evict_acp_task_after_terminal_error`]
//! 的泛化:那条路径在 ACP 终态错误后 kill 任务,这条路径换模型后重建并交回新句柄。

use std::sync::Arc;

use nomifun_api_types::{HealthStatus, ModelHealthStatus};
use nomifun_common::{AgentKillReason, AgentType, ErrorChain, ProviderWithModel, now_ms};
use nomifun_db::{ConversationRowUpdate, UpdateProviderParams};
use nomifun_ai_agent::{AgentInstance, IWorkerTaskManager};
use tracing::{info, warn};

use crate::convert::string_to_enum;
use crate::model_failover::{
    get_global_failover_config, next_failover_model, read_conversation_failover_override,
};
use crate::service::{ConversationService, parse_conv_id};
use crate::stream_relay::RelayOutcome;
use crate::task_options::provider_model_from_conversation_row;

/// 一次成功的故障转移结果:重建后的新任务句柄 + 被选中的候选模型。
pub struct FailoverSwitch {
    /// 换模型并重建后的 agent 句柄。send-loop 用它 `subscribe()` + 重发同一内容。
    pub agent: AgentInstance,
    /// 本次切换到的 `(provider_id, model)`(已写入 `conversation.model`)。
    pub picked: ProviderWithModel,
}

impl ConversationService {
    /// 解析该会话**生效**的故障转移配置:会话级 `extra.model_failover` 覆盖存在
    /// 则优先,否则回落到全局 `client_preferences` 的 `agent.model_failover`。
    /// 未注册 client-prefs 依赖(`with_failover_deps` 没调过)时返回 `None` —— 视为
    /// 故障转移关闭(fail-safe)。
    pub(crate) async fn resolve_failover_config(
        &self,
        extra_json: &str,
    ) -> Option<nomifun_api_types::ModelFailoverConfig> {
        if let Some(override_cfg) = read_conversation_failover_override(extra_json) {
            return Some(override_cfg);
        }
        let (_, client_prefs) = self.failover_deps()?;
        Some(get_global_failover_config(&client_prefs).await)
    }

    /// 把失败模型的 `model_health[model]` 标 `Unhealthy`(read-改-write,保留其余
    /// 模型的健康记录)。fail-open:任何一步出错只 warn 不致命 —— 标记是尽力而为的
    /// 加分项,不能拖垮故障转移本身。
    async fn stamp_model_unhealthy(&self, failed: &ProviderWithModel) {
        let Some((provider_repo, _)) = self.failover_deps() else {
            return;
        };
        let provider = match provider_repo.find_by_id(&failed.provider_id).await {
            Ok(Some(provider)) => provider,
            Ok(None) => {
                warn!(provider_id = %failed.provider_id, "Failover stamp-unhealthy skipped: provider row missing");
                return;
            }
            Err(e) => {
                warn!(error = %ErrorChain(&e), provider_id = %failed.provider_id, "Failover stamp-unhealthy: failed to load provider");
                return;
            }
        };

        let mut health: std::collections::HashMap<String, ModelHealthStatus> = provider
            .model_health
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        health.insert(
            failed.model.clone(),
            ModelHealthStatus {
                status: HealthStatus::Unhealthy,
                last_check: Some(now_ms()),
                latency: None,
                error: Some("model_failover: provider fault on live turn".into()),
            },
        );
        let serialized = match serde_json::to_string(&health) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %ErrorChain(&e), "Failover stamp-unhealthy: serialize model_health failed");
                return;
            }
        };
        let params = UpdateProviderParams {
            model_health: Some(Some(serialized.as_str())),
            ..Default::default()
        };
        if let Err(e) = provider_repo.update(&failed.provider_id, params).await {
            warn!(error = %ErrorChain(&e), provider_id = %failed.provider_id, "Failover stamp-unhealthy: provider update failed");
        }
    }

    /// **核心、可复用**的故障转移动作(plan D3 的「Some(next)」分支主体):
    /// 挑下一候选 → 写 `conversation.model`(origin 标记,非用户编辑)→
    /// (`stamp_unhealthy` 则)标失败模型 Unhealthy → `kill_and_wait`(镜像
    /// [`Self::evict_acp_task_after_terminal_error`])→ 用刷新后的行
    /// `build_task_options` 重建任务。返回 `Some(FailoverSwitch)` 表示换好新模型、
    /// 新句柄就绪;返回 `None` 表示**队列耗尽**(无可用候选)—— 调用方据此回落到
    /// 「emit 原始错误」,绝不无限切换。
    ///
    /// send-loop(D3)与 IDMM 故障值守(D6)共用此方法:一份实现,两处触发。
    ///
    /// **ACP 边界(review #9,plan D7)**:加载会话行后在此**统一**判定 agent 类型——
    /// 仅 `AgentType::Nomi` 放行,其余(ACP / 终端 CLI / 远程 …)`warn` + 返回 `None`
    /// (不 kill、不写 model)。send-loop 自己也有一道便宜的早闸,但**这里**才是
    /// 唯一的强制点:send-loop 与 IDMM 两条路径都过这道闸,所以 ACP 会话无论从哪条
    /// 路径进来都安全地被拒。
    ///
    /// 注意:这里只换模型 + 重建 + 交回句柄,**不**负责重发消息 —— 重发是触发方
    /// (send-loop 重发同一 `current_send`;IDMM 自行决定)的职责。
    ///
    /// `tried` 是本轮**已经切到过**的候选(review #2 单调性):挑选器跳过它们,
    /// 多次切换不回头重试同一候选。send-loop 累积本轮 picks 后传入;IDMM 单次切换
    /// 传空切片即可(它每次 `WakeAction::Failover` 只切一次,无跨回合累积)。
    pub async fn perform_model_failover(
        &self,
        conversation_id: &str,
        config: &nomifun_api_types::ModelFailoverConfig,
        tried: &[ProviderWithModel],
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Option<FailoverSwitch> {
        let Some((provider_repo, _)) = self.failover_deps() else {
            return None;
        };
        let conv_id = parse_conv_id(conversation_id).ok()?;
        let row = match self.conversation_repo().get(conv_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                warn!(conversation_id, "Failover skipped: conversation row missing");
                return None;
            }
            Err(e) => {
                warn!(error = %ErrorChain(&e), conversation_id, "Failover skipped: failed to load conversation");
                return None;
            }
        };

        // ACP 边界(review #9,plan D7)的**唯一强制闸**:仅 nomi 自有引擎的普通会话
        // 可换模型重建。ACP / 终端 / 远程等 agent 自管模型(独立 reconcile),在此被
        // fail-safe 拒绝——不 kill、不写 model。send-loop 与 IDMM inject 都走这条
        // 路径,故两处都被这一道闸覆盖。
        let agent_type: AgentType = match string_to_enum(&row.r#type) {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %ErrorChain(&e), conversation_id, agent_type = %row.r#type, "Failover skipped: unparseable agent type");
                return None;
            }
        };
        if agent_type != AgentType::Nomi {
            warn!(
                conversation_id,
                agent_type = ?agent_type,
                "Failover skipped: not a nomi conversation (ACP/terminal self-manage their model)"
            );
            return None;
        }

        let failed = provider_model_from_conversation_row(&row);
        let providers = match provider_repo.list().await {
            Ok(providers) => providers,
            Err(e) => {
                warn!(error = %ErrorChain(&e), conversation_id, "Failover skipped: failed to list providers");
                return None;
            }
        };

        // 队列耗尽 / 无可用候选 → None(调用方回落到原始错误)。
        let picked = next_failover_model(&config.queue, &failed, tried, &providers)?;

        // 写 conversation.model(origin 标记:非用户编辑)。这正是 spec §5.5 锚定的
        // service.rs:896-927「改模型 + kill → 下次 send 重建」形状,只是这里立刻重建。
        // 同时这会改掉 IDMM 默认 bypass 模型(可接受:换走的正是那个故障模型)。
        let model_json = match serde_json::to_string(&picked) {
            Ok(json) => json,
            Err(e) => {
                warn!(error = %ErrorChain(&e), conversation_id, "Failover aborted: serialize picked model failed");
                return None;
            }
        };
        let update = ConversationRowUpdate {
            model: Some(Some(model_json)),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        if let Err(e) = self.conversation_repo().update(conv_id, &update).await {
            warn!(error = %ErrorChain(&e), conversation_id, "Failover aborted: failed to persist new model");
            return None;
        }

        if config.stamp_unhealthy {
            self.stamp_model_unhealthy(&failed).await;
        }

        info!(
            conversation_id,
            failed_provider = %failed.provider_id,
            failed_model = %failed.model,
            next_provider = %picked.provider_id,
            next_model = %picked.model,
            reason = ?AgentKillReason::AgentErrorRecovery,
            "Model failover: switching model and rebuilding task"
        );

        // kill_and_wait,镜像 evict_acp_task_after_terminal_error(acp_error_recovery.rs):
        // 旧任务句柄绑定旧 provider/model,必须等它落幕再用新行重建。
        task_manager
            .kill_and_wait(conversation_id, Some(AgentKillReason::AgentErrorRecovery))
            .await;

        // 用**刷新后**的行重建。re-fetch 以拿到刚写入的新 model 列。
        let refreshed = match self.conversation_repo().get(conv_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                warn!(conversation_id, "Failover aborted: conversation vanished after model write");
                return None;
            }
            Err(e) => {
                warn!(error = %ErrorChain(&e), conversation_id, "Failover aborted: re-fetch after model write failed");
                return None;
            }
        };
        let build_opts = match self.build_task_options(&refreshed) {
            Ok(opts) => opts,
            Err(e) => {
                warn!(error = %ErrorChain(&e), conversation_id, "Failover aborted: build_task_options on refreshed row failed");
                return None;
            }
        };
        let agent = match task_manager.get_or_build_task(conversation_id, build_opts).await {
            Ok(agent) => agent,
            Err(e) => {
                warn!(error = %ErrorChain(&e), conversation_id, "Failover aborted: rebuild task failed");
                return None;
            }
        };

        Some(FailoverSwitch { agent, picked })
    }

    /// 同模型"剔图重建":标记 registry(该 provider+model 不支持图片)→ kill →
    /// 用同一行重建任务。重建时工厂重新读 registry → compat.supports_image=false →
    /// build_messages 剔图。仅 nomi 会话放行;返回新句柄或 None(不可重建)。
    pub(crate) async fn strip_images_and_rebuild(
        &self,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Option<AgentInstance> {
        let conv_id = parse_conv_id(conversation_id).ok()?;
        let row = match self.conversation_repo().get(conv_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                warn!(conversation_id, "strip_images_and_rebuild skipped: conversation row missing");
                return None;
            }
            Err(e) => {
                warn!(error = %ErrorChain(&e), conversation_id, "strip_images_and_rebuild skipped: load failed");
                return None;
            }
        };
        let agent_type: AgentType = string_to_enum(&row.r#type).ok()?;
        if agent_type != AgentType::Nomi {
            return None;
        }
        let pm = provider_model_from_conversation_row(&row);
        nomifun_common::VisionUnsupportedRegistry::global().mark_unsupported(&pm.provider_id, &pm.model);

        task_manager
            .kill_and_wait(conversation_id, Some(AgentKillReason::AgentErrorRecovery))
            .await;

        let build_opts = match self.build_task_options(&row) {
            Ok(opts) => opts,
            Err(e) => {
                warn!(error = %ErrorChain(&e), conversation_id, "strip_images_and_rebuild aborted: build_task_options failed");
                return None;
            }
        };
        match task_manager.get_or_build_task(conversation_id, build_opts).await {
            Ok(agent) => Some(agent),
            Err(e) => {
                warn!(error = %ErrorChain(&e), conversation_id, "strip_images_and_rebuild aborted: rebuild failed");
                None
            }
        }
    }

    /// send-loop(plan D3)的故障转移决策入口:在 `consume_with_send_error` 之后调用。
    /// **全部满足**才转移(否则返回 `None`,send-loop 按现状 emit 原始错误):
    /// 1. terminal 是 Error 且 code 命中 [`crate::model_failover::is_provider_fault`];
    /// 2. **pre-response**:本轮未吐任何 assistant Text / 工具动作
    ///    (`!outcome.emitted_response`,plan D4 + review #4)—— 杜绝重复输出 /
    ///    重复副作用 / 重复计费;
    /// 3. 故障转移启用(会话级覆盖否则全局,`enabled == true`);
    /// 4. `switches_done < min(max_switches, queue.len())` —— bounded;
    /// 5. agent 是 **nomi** 实例(plan D7;终端 CLI / ACP 自管模型,排除)。
    ///
    /// 命中且挑到可用候选 → 换模型 + 重建,返回 `Some(FailoverSwitch)`;
    /// 任一条件不满足 / 队列耗尽 → `None`。
    ///
    /// **不变量**:user-cancel 不会进到这里(取消是 `RelayTerminal::ChannelClosed`
    /// 或非 provider-fault 码,`is_provider_fault` 与 `is_error` 双重过滤);
    /// mid-response 故障被第 2 条挡掉(emit 错误,不转移)。
    pub(crate) async fn maybe_failover_in_send_loop(
        &self,
        conversation_id: &str,
        agent_type: AgentType,
        outcome: &RelayOutcome,
        switches_done: u32,
        tried: &[ProviderWithModel],
        extra_json: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Option<FailoverSwitch> {
        // (5) 仅 nomi 自有引擎的普通会话。便宜的早闸(避免无谓加载);真正的强制点
        //     在 `perform_model_failover` 的 ACP 边界闸(review #9),send-loop 与 IDMM
        //     共用那一处。
        if agent_type != AgentType::Nomi {
            return None;
        }
        // (1) provider 故障的终态错误。
        let RelayOutcome {
            terminal,
            emitted_response,
            ..
        } = outcome;
        if !terminal.is_error() {
            return None;
        }
        let Some(code) = terminal.code() else {
            return None;
        };
        if !crate::model_failover::is_provider_fault(code) {
            return None;
        }
        // (2) pre-response:本轮已吐过 Text / 工具动作则不转移(post-response 故障 →
        //     emit 错误,杜绝重复输出 / 重复副作用 / 重复计费)。
        if *emitted_response {
            return None;
        }
        // (3) 启用?(会话级覆盖否则全局)
        let config = self.resolve_failover_config(extra_json).await?;
        if !config.enabled {
            return None;
        }
        // (4) bounded:受 max_switches 与队列长度双重封顶。
        let bound = config.max_switches.min(config.queue.len() as u32);
        if switches_done >= bound {
            warn!(
                conversation_id,
                switches_done,
                max_switches = config.max_switches,
                queue_len = config.queue.len(),
                "Model failover bound reached; surfacing original error"
            );
            return None;
        }

        self.perform_model_failover(conversation_id, &config, tried, task_manager)
            .await
    }

    /// IDMM 故障值守(plan D6)的故障转移入口。`maybe_failover_in_send_loop` 是
    /// send-loop 的进入条件闸(pre-response / bounded / nomi-only 都已由 send-loop
    /// 上下文保证);**这条**是 IDMM 探针(`ConversationProbe::inject(Failover)`)的
    /// 进入条件闸:此刻没有活跃 send-loop,IDMM 自己是触发方,故由本方法
    /// 解析配置 → 调用**同一个** [`Self::perform_model_failover`] 换模型重建 →
    /// 重新驱动本轮(发一条 hidden 续聊消息,镜像 inject 的 Retry 路径)。
    ///
    /// 返回 `Ok(true)` = 成功切到下一候选并已重新驱动;`Ok(false)` = 未转移
    /// (故障转移关闭 / 队列耗尽 / 依赖未注册 / 非 nomi),调用方据此回落(不无限切换)。
    ///
    /// **IDMM 切换次数边界(review #3)**:本方法**每次** `WakeAction::Failover` 只执行
    /// **一次**模型切换(调一次 `perform_model_failover`),不持有任何跨回合计数器,也
    /// 不读 `max_switches`(那是 send-loop 单轮内自重发的封顶)。IDMM 路径下的总切换
    /// 次数由**故障值守自身的 `fault_watch.max_retries`** 间接封顶:值守每观察到一次
    /// provider 故障最多发一次 `Failover`,`max_retries` 用尽后值守 ladder 升级 / 兜底,
    /// 不再发 `Failover`。故无需也不应在此另设跨回合计数。
    ///
    /// **不变量**:与 send-loop 共用 `perform_model_failover` 这一份实现(no
    /// duplicate);队列耗尽 → `Ok(false)`(IDMM 不再自动切换,值守 ladder 继续按
    /// 现状把它当 provider 故障处理 / 升级 / 兜底)。ACP 边界由 `perform_model_failover`
    /// 内部统一闸守(review #9):非 nomi 会话在那里返回 `None` → 本方法 `Ok(false)`。
    pub async fn idmm_failover_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<bool, nomifun_common::AppError> {
        let conv_id = parse_conv_id(conversation_id)?;
        let extra_json = match self.conversation_repo().get(conv_id).await {
            Ok(Some(row)) => row.extra,
            Ok(None) => {
                warn!(conversation_id, "IDMM failover skipped: conversation row missing");
                return Ok(false);
            }
            Err(e) => return Err(nomifun_common::AppError::from(e)),
        };

        let Some(config) = self.resolve_failover_config(&extra_json).await else {
            return Ok(false);
        };
        if !config.enabled {
            return Ok(false);
        }

        // 同一份换模型 + 重建实现(send-loop 也调它)。None = 队列耗尽 → 不转移。
        // IDMM 每次 Failover 只切一次,无跨回合累积,故 `tried` 传空切片(review #2)。
        if self
            .perform_model_failover(conversation_id, &config, &[], task_manager)
            .await
            .is_none()
        {
            return Ok(false);
        }

        // 换好新模型 + 重建句柄后,重新驱动本轮:发一条 hidden 续聊消息,镜像
        // `ConversationProbe::inject(Retry)` 的 send_message(origin="idmm")路径。
        let req = nomifun_api_types::SendMessageRequest {
            content: "Please continue.".to_string(),
            files: vec![],
            inject_skills: vec![],
            hidden: true,
            origin: Some("idmm".into()),
            channel_platform: None,
        };
        self.send_message(user_id, conversation_id, req, task_manager)
            .await
            .map(|_| true)
    }
}

# Agent 集群特化增强 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (本会话内联执行，执行者已持有全量调研上下文). Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 spec `docs/superpowers/specs/2026-07-06-agent-cluster-enhancement-design.md`——「agent 集群」入口回归、画布精美化+性能、主 agent 实时反馈、main 节点移除、节点级审批模式。

**Architecture:** 前端 React (arco + unocss + CSS modules)，后端 Rust workspace。编排 run 由 lead 经 MCP 工具铸造；会话可见反馈唯一通道是 LeadReporter→steer_message 隐藏回执；画布经 WS 事件驱动 useRunLive refetch。本计划在既有通道上做增量：新增意图标记 + 提示升级、needs_review 节点级审批门（Phase D 正式落地）、前端 WS 实时进度层。

**Tech Stack:** React 18 / @xyflow/react v12 / arco-design / unocss / i18next；Rust tokio / sqlx(sqlite) / axum。

## Global Constraints

- @icon-park/react 导入禁用 `as` 别名（IconParkHOC babel 插件会崩）。
- 前端验证必须 `bun run build`（不只 tsc）；i18n 改动 zh-CN 与 en-US 同步 + 重新生成 i18n-keys.d.ts。
- 主题变量 only，节点/画布无硬编码 hex（MiniMap JS 色除外，双主题字面量镜像 taskStatusMeta）。
- per-run 锁绝不跨 LLM await；lead 回执 best-effort（失败只 warn）；`lead_conv_id=None` 不回执。
- 迁移 append-only 纯 ADD COLUMN；旧行读回 NULL 零回归。下一个迁移号：**030**。
- 编排能力对 Remote 面硬拒（ORCHESTRATOR_DENY_SURFACES）；新工具同样 deny。
- NomiChat 必须始终挂载（display:none 切换）；无 Provider 表面直通/渲染 null（伙伴零改动）。
- commit author=`nomifun <rika00@qq.com>`，无 Co-Authored-By；分支 feature/agent-cluster。

---

## Phase 1 — 后端

### Task 1: 集群模式提示（agent_cluster_mode → CLUSTER_MODE_HINT）

**Files:**
- Modify: `crates/backend/nomifun-api-types/src/agent_build_extra.rs`（NomiBuildExtra，~line 342 后加字段）
- Modify: `crates/backend/nomifun-ai-agent/src/factory/nomi.rs`（~line 190 注入点 + ~line 777 纯函数区 + tests mod）

**Interfaces:**
- Produces: `NomiBuildExtra.agent_cluster_mode: bool`（serde default，alias `agentClusterMode`）；
  `pub(crate) const CLUSTER_MODE_HINT: &str`；
  `compose_subagent_hint(base, inject, cluster: bool)` 第三参（cluster=true 时在 SUBAGENT_STANDARD_HINT 之后再追加 CLUSTER_MODE_HINT；cluster 只在 inject=true 时生效）。

- [ ] Step 1: NomiBuildExtra 加字段：
```rust
/// 「agent 集群」意图标记（需求1）。用户在 composer 显式点选后写到会话 extra；
/// 工厂据此在常驻 subagent 提示之上追加更强的 CLUSTER_MODE_HINT（必须刻意评估
/// 是否开集群、太简单须先说明原因）。仅塑形提示，不授予能力。
#[serde(default, alias = "agentClusterMode")]
pub agent_cluster_mode: bool,
```
- [ ] Step 2: nomi.rs 加常量（放 SUBAGENT_STANDARD_HINT 之后）：
```rust
/// 「agent 集群」模式增强提示（需求1）。仅当会话 extra.agent_cluster_mode=true 且
/// 常驻 subagent 提示已注入（同一网关前提）时，追加在其后。
pub(crate) const CLUSTER_MODE_HINT: &str = "用户已为本会话显式开启「agent 集群」模式：对每一个任务（无论难度），你都必须先刻意评估是否应当用 nomi_spawn / nomi_run_create 开启多 agent 集群协作，并倾向于开启——多个独立 agent 各自拥有充足上下文，交付质量更高。只有当任务确实过于简单（单步可答、无可拆分子任务）时才可不开启，但此时必须在回复的开头先用一两句话向用户说明「本次使用简单模式」的原因，然后再直接作答。";
```
- [ ] Step 3: `compose_subagent_hint(base: Option<String>, inject: bool, cluster: bool)`：inject=false 原样返回；inject=true 拼 SUBAGENT_STANDARD_HINT；cluster=true 再拼 `\n\n{CLUSTER_MODE_HINT}`。调用点（~line 190）传 `overrides.agent_cluster_mode`。
- [ ] Step 4: tests mod 更新既有 compose 测试签名 + 新增：cluster=true 含两段提示；cluster=true 但 inject=false 不含任何提示。
- [ ] Step 5: `cargo test -p nomifun-ai-agent` 全绿 → commit `feat(cluster): 会话 extra 集群模式标记 + 提示注入`

### Task 2: 迁移 030 + approval_mode/pending_question 数据面

**Files:**
- Create: `crates/backend/nomifun-db/migrations/030_cluster_approval.sql`
- Modify: `crates/backend/nomifun-db/src/...`（OrchRunRow/OrchRunTaskRow + sqlite repo：SELECT/INSERT 列表、`set_task_question` 新方法）
- Modify: `crates/backend/nomifun-api-types/src/orchestrator.rs`（Run.approval_mode、RunTask.pending_question、CreateAdhocRunRequest.approval_mode）
- Modify: `crates/backend/nomifun-orchestrator/src/run_service.rs`（create_adhoc 落列，默认 "auto"）
- Modify: `crates/backend/nomifun-gateway/src/caps_orchestrator.rs`（create/spawn 读会话 extra `orchestrator_approval_mode` → request，仿 read_conversation_model_range）

**Interfaces:**
- Produces: `Run.approval_mode: String`（serde default "auto"；合法值 "auto"|"manual"）；
  `RunTask.pending_question: Option<String>`；
  repo `set_task_question(task_id: &str, question: Option<&str>) -> Result<(), AppError>`；
  gateway `read_conversation_approval_mode(deps, user, conversation_id) -> Option<String>`。

- [ ] Step 1: 迁移 SQL：
```sql
-- 030 agent集群：节点级审批模式。append-only,基线不动。
-- orch_runs.approval_mode: NULL/"auto"=全授权(节点遇抉择自行判断,现状零回归);
--   "manual"=审批模式(worker 可经 nomi_task_question 挂起提问,人来选)。
-- orch_run_tasks.pending_question: 节点挂起的决策问题原文(needs_review 态);
--   解决(采用产出/重跑)后清空。
ALTER TABLE orch_runs ADD COLUMN approval_mode TEXT;
ALTER TABLE orch_run_tasks ADD COLUMN pending_question TEXT;
```
- [ ] Step 2: row 结构 + repo SELECT/INSERT/DTO 映射补两列（NULL→"auto"/None）；新增 `set_task_question`（单 UPDATE，顺带 touch updated_at）。
- [ ] Step 3: CreateAdhocRunRequest 加 `approval_mode: Option<String>`，create_adhoc 落库（None→"auto"）；replan 保留原值。
- [ ] Step 4: caps_orchestrator：`read_conversation_approval_mode`（读会话 extra 的 `orchestrator_approval_mode`，仅接受 "manual"，其余 None）；create(~line 292 附近) 与 spawn(~line 1129 附近) 都接上。
- [ ] Step 5: 单测：request 透传（仿 build_adhoc_request 既有测试）+ repo 读写 roundtrip。`cargo test -p nomifun-db -p nomifun-orchestrator -p nomifun-gateway` → commit `feat(cluster): 迁移030 审批模式与节点提问数据面`

### Task 3: RunOutcome::NodeQuestion + 回执文案

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/engine.rs`（RunOutcome enum ~line 121-158）
- Modify: `crates/backend/nomifun-app/src/router/state.rs`（compose_lead_receipt ~line 1079 + tests）

**Interfaces:**
- Produces: `RunOutcome::NodeQuestion`（as_str `"node_question"`），非终态。

- [ ] Step 1: enum 加变体 + as_str。
- [ ] Step 2: compose_lead_receipt 加 arm：
```rust
RunOutcome::NodeQuestion => format!(
    "[编排回执·节点提问] 你派发的编排（run {run_id}）中有一个节点遇到决策问题、已挂起等待人工选择：\n{brief}\n\
     本 run 处于「审批模式」：请立即用一段话提醒用户——该节点已在画布与进度条上标出提问图标，\
     请用户点击进入该节点会话，直接查看问题并回复选择；回复后在该节点点「采用为该节点产出」即可继续。\
     你自己不要替用户作答，也不要重复创建编排。"
),
```
- [ ] Step 3: state.rs 纯函数单测（含 brief 注入、autonomous 不劫持该 arm——NodeQuestion 非 Completed/Failed 天然绕过 autonomous 分支）。
- [ ] Step 4: `cargo test -p nomifun-app compose_lead_receipt` → commit `feat(cluster): NodeQuestion 回执`

### Task 4: nomi_task_question 工具 + 引擎 park/豁免/恢复

**Files:**
- Modify: `crates/backend/nomifun-gateway/src/caps_orchestrator.rs`（新 handler + register）
- Modify: `crates/backend/nomifun-orchestrator/src/engine.rs`（settle 守卫、终态判定、看门狗豁免、brief 决策策略段）
- Modify: `crates/backend/nomifun-orchestrator/src/run_service.rs`（adopt/rerun 清 pending_question）

**Interfaces:**
- Produces: 网关工具 `nomi_task_question { question: string }`——仅 worker 会话（extra 携 orchestrator run_id/task_id）可用；写 pending_question + status→needs_review + emit task.statusChanged + LeadReporter.report(NodeQuestion)。
- 引擎不变量：settle 遇 task.status=="needs_review" → 仅落 output_summary/conversation_id/tokens，不改状态、不 emit done；终态判定把 needs_review 视为「未 settled 且不可判 failed」→ 循环退出但 run 保持 running；看门狗对含 needs_review 任务的 running run 豁免 stalled；adopt（已有）与 rerun 清 pending_question（needs_review→done / →pending）。

- [ ] Step 1: 先读 engine.rs 终态判定分支与 reap_stalled_runs 全文、worker brief 组装点、run_service adopt/rerun 实现（grep 定位，超大文件勿整读）。
- [ ] Step 2: settle_task_outcome `Ok(o) if o.ok` 分支入口加守卫：re-read task；若 status=="needs_review" → 只写 conversation_id/output_summary/tokens（不写 status、不 emit done）后 return。
- [ ] Step 3: 终态判定：needs_review 不计入 done/failed/skipped 集合；存在 needs_review 且无 inflight/ready 时走「保持 running、退出循环」分支（复用 awaiting_plan_approval 的干净退出样式，engine.rs:1136-1147 同款注释说明由 adopt/rerun 重启）。
- [ ] Step 4: reap_stalled_runs：跳过含 needs_review 任务的 run（列表页读 tasks，与既有查询合并，避免 N+1——若已有 per-run task 读取则顺路）。
- [ ] Step 5: worker brief 组装点按 run.approval_mode 追加：manual→「遇到会显著影响方向/取舍的决策问题时，调用 nomi_task_question(question) 提交问题并立即结束本轮等待人工选择；不要自行猜测。」auto→「遇到抉择自行选择最合理方案并在产出中说明理由，不要停下来提问。」
- [ ] Step 6: caps_orchestrator 新 handler `task_question`：从 CallerCtx.conversation_id 读会话 extra 拿 run_id/task_id（worker.rs 建会话时已埋）；校验 run.approval_mode=="manual"（否则报错文案引导自行决策）；`set_task_question` + `update_task(status=needs_review)` + `emit_task_status` + `report(NodeQuestion, brief=「节点『{title}』：{question}」)`；register() 注册（描述英文、注明仅审批模式 worker 可用）。
- [ ] Step 7: adopt_task_result 与 rerun_task 里清 pending_question（set_task_question(None)），并确认 adopt 的状态守卫接受 needs_review 起点。
- [ ] Step 8: 引擎单测（engine.rs tests mod，复用 reporter_stack/adhoc_lead_run 范式）：(a) worker 把 task 置 needs_review 后 settle 不覆盖为 done、run 保持 running 且循环退出；(b) 看门狗不 reap 含 needs_review 的 run；(c) adopt 后 run 重启至 completed。
- [ ] Step 9: `cargo test -p nomifun-orchestrator -p nomifun-gateway` → commit `feat(cluster): 节点级审批模式（nomi_task_question + needs_review park）`

### Task 5: 批量进展回执放宽（3→1）

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/engine.rs`（BATCH_REPORT_MIN_NODES ~line 252 + doc + 测试 ~line 4709）

- [ ] Step 1: `const BATCH_REPORT_MIN_NODES: usize = 1;` 注释更新（间隔节流仍 20s；需求4：中途每批交付最迟 20s 内回执转述）。
- [ ] Step 2: 更新 `batch_progress_reports_to_lead_midrun` 及相邻断言（原按 3 个节点设计的场景改为断言更早触发 + 间隔仍然节流）。
- [ ] Step 3: `cargo test -p nomifun-orchestrator batch` → commit `feat(cluster): 中途批量回执降门槛到1节点(20s节流保留)`

## Phase 2 — 前端

### Task 6: 类型镜像

**Files:**
- Modify: `ui/src/common/types/orchestrator/orchestratorTypes.ts`（TRun.approval_mode?、TRunTask.pending_question?）

- [ ] 加可选字段 + 注释 → 与 Task 7+ 同 commit 亦可，单独提交 `feat(cluster): 前端类型镜像`。

### Task 7: 渲染性能（单一数据源 + 去抖 + 廉价签名 + 缓存清理）

**Files:**
- Modify: `ui/src/renderer/pages/orchestrator/useRunLive.ts`（去抖合并）
- Modify: `ui/src/renderer/pages/orchestrator/RunDetail/DagCanvas.tsx`（props 接 detail/loading/refetch；删内部 useRunLive；签名函数；nodeCacheRef 清理）
- Modify: `ui/src/renderer/pages/conversation/orchestration/OrchestrationTopPanel.tsx`（传 detail/loading/refetch）

**Interfaces:**
- `DagCanvasProps` 变为 `{ runId, detail: TRunDetail | null, loading: boolean, refetch: () => Promise<void>, onOpenTask, activeTaskId? }`（onOpenMain/mainActive 在 Task 8 删）。

- [ ] Step 1: useRunLive：事件 → `scheduleRefetch()`（180ms trailing timer；fetch 在飞则置 dirty，finally 时若 dirty 再补一次）；卸载清 timer；refetch 保持手动即时（approve/rerun 路径显式 await 不受去抖影响）。
- [ ] Step 2: DagCanvas 改 props；OrchestrationTopPanel 从 context 解构传入。
- [ ] Step 3: 签名：`sigForTask(task, assignment, member, selected, pos)` 手工拼字段串替换 JSON.stringify；构建结束后 `for (const key of cache.keys()) if (!liveIds.has(key)) cache.delete(key)`。
- [ ] Step 4: `bun run build` → commit `perf(cluster): 画布单一数据源+去抖refetch+廉价签名+缓存清理`

### Task 8: 移除 main 节点

**Files:**
- Modify: `ui/src/renderer/pages/orchestrator/RunDetail/DagCanvas.tsx`（删 MAIN_NODE_ID/MAIN_ROW_OFFSET/main 注入与 main 边/NODE_TYPES_WITH_MAIN/onOpenMain/mainActive）
- Delete: `ui/src/renderer/pages/orchestrator/RunDetail/nodes/MainNode.tsx`
- Modify: `ui/src/renderer/pages/conversation/orchestration/OrchestrationTopPanel.tsx`（去掉两 prop）
- Modify: i18n orchestrator.json（删 run.detail.canvas.mainNode，zh+en）

- [ ] 删干净后 `bun run build` + grep 确认无 MainNode 引用 → commit `feat(cluster): 画布移除 main agent 合成节点`

### Task 9: 画布 UI 精美化

**Files:**
- Modify: `ui/src/renderer/pages/orchestrator/RunDetail/nodes/TaskNode.tsx`（卡片结构重构 + 提问徽标）
- Modify: `ui/src/renderer/pages/orchestrator/RunDetail/dag-canvas.css`（hover/active/selected/入场/流光/呼吸样式）
- Modify: `ui/src/renderer/pages/orchestrator/RunDetail/DagCanvas.tsx`（data 传 pendingQuestion、入场 stagger index、规划占位叙事接 leadThinking props）
- Modify: `ui/src/renderer/pages/conversation/orchestration/OrchestrationTopPanel.tsx`（leadThinking 传入画布占位）

**要点（全部主题变量）：**
- `.nomi-dag-card`：rest `box-shadow` 低、hover 抬升(translateY(-2px)+阴影加深+边框 primary 淡化)、active scale(0.985)、transition 180ms cubic-bezier(.2,.8,.2,1)。
- running：左侧状态条纹 + 卡片柔和 glow（`box-shadow: 0 0 0 1px color-mix(primary 30%), 0 0 18px color-mix(primary 18%)` 呼吸动画）+ 顶部 2px shimmer 进度条。
- selected：animated ring（primary，脉冲一次后常亮）。
- needs_review + pendingQuestion：警示徽标（Help/Caution 图标 + `nomi-dag-question-pulse` 缓脉冲）+ 琥珀 ring。
- 入场：`.nomi-dag-enter { animation: nomi-dag-enter .32s both } @keyframes 从 opacity 0/translateY(6px)/scale(.97)`，`animation-delay: calc(var(--dag-i) * 40ms)`（node style 传 `--dag-i`，上限 12）。
- 边：running 渐变流光（stroke-dasharray + dashoffset 动画类 `.nomi-dag-edge-live`）。
- 规划占位：phaseKeys → i18n 文案列表逐条淡入 + reasoning 尾部 2 行滚动。
- 签名函数补入 pendingQuestion/needs_review 相关字段。
- [ ] `bun run build` → commit `feat(cluster): 画布节点/边/入场/规划叙事精美化 + 提问徽标`

### Task 10: ClusterProgressStrip（会话内实时进度层）

**Files:**
- Create: `ui/src/renderer/pages/conversation/orchestration/ClusterProgressStrip.tsx`
- Create: `ui/src/renderer/pages/conversation/orchestration/clusterProgressStrip.module.css`
- Modify: `ui/src/renderer/pages/conversation/components/ChatConversation.tsx`（PlanApprovalBanner 下方挂载）

**Interfaces:**
- 消费 `useOrchestrationSafe()`：null/无 run → null。渲染：run 状态点 + 「agent 集群 · N/M 节点」+ 规划中 leadThinking 阶段 + 横向节点 chips（状态色点+标题+needs_review 提问图标）+ 提问横幅（存在 pending_question 节点时）。
- chip/横幅点击 → 用 context.detail 组装 `OpenTaskPayload{task, assignment: detail.assignments.find, fleetMembers: detail.fleet_members, runId, refetch}` → `projectTask(payload)`。
- 可折叠（记 localStorage `nomifun:cluster-strip-collapsed`）。
- [ ] `bun run build` → commit `feat(cluster): 会话内集群实时进度条`

### Task 11: ProjectedWorkerView 提问横幅

**Files:**
- Modify: `ui/src/renderer/pages/conversation/orchestration/ProjectedWorkerView.tsx`（task.pending_question 存在时 banner 下方显问题原文 + 「回复后点采用为该节点产出」提示）

- [ ] `bun run build` → commit `feat(cluster): 节点投影视图提问横幅`

### Task 12: 入口按钮（guid）+ extra 透传

**Files:**
- Modify: `ui/src/renderer/pages/guid/components/ComposerEntryStrip.tsx`（默认态最左新增「agent 集群」toggle：props `clusterActive: boolean; onToggleCluster: () => void`；图标用 icon-park 集群语义图标，无 as 别名；active 用 entryButtonActive）
- Modify: `ui/src/renderer/pages/guid/GuidPage.tsx`（clusterMode state + 接线 + 发送链路）
- Modify: guid 发送 hook（useGuidSend 或现行创建路径）：创建会话 extra 合并 `agent_cluster_mode: true`
- Modify: `ui/src/renderer/pages/guid/components/ComposerEntryStrip.test.ts`

- [ ] 单测更新 + `bun run build` → commit `feat(cluster): 首页 agent集群 入口按钮`

### Task 13: 会话页集群 pill（模式 + 审批模式）

**Files:**
- Create: `ui/src/renderer/pages/conversation/components/ClusterModePill.tsx`
- Modify: `ui/src/renderer/pages/conversation/components/ChatConversation.tsx`（与 collaboratorSelectorNode 同路注入 NomiChat composer）

**Interfaces:**
- 读 conversation.extra.agent_cluster_mode / orchestrator_approval_mode；popover 内两开关：「agent 集群」（写 extra.agent_cluster_mode）+「节点审批模式」（写 extra.orchestrator_approval_mode: 'manual'|删除）；写回走 `ipcBridge.conversation.update`（extra 顶层浅合并，只覆盖本键——照抄 orchestrator_model_range 写法 ChatConversation.tsx:177-189）。
- [ ] `bun run build` → commit `feat(cluster): 会话页集群模式/审批模式 pill`

### Task 14: i18n 全量 + d.ts

**Files:**
- Modify: `ui/src/renderer/services/i18n/locales/zh-CN/{guid,conversation,orchestrator}.json` + en-US 同名
- 重生成 i18n-keys.d.ts（`bun run check:i18n` 或仓库现行命令）

- [ ] 收口所有新 key（guid.entry.cluster、conversation.cluster.*、orchestrator.run.question.* 等，前述任务用 defaultValue 先行，此处正式登记）→ commit `chore(cluster): i18n zh/en 与类型`

## Phase 3 — 验证与审查

### Task 15: 全量验证
- [ ] `bun run build`；`cargo test -p nomifun-orchestrator -p nomifun-gateway -p nomifun-app -p nomifun-ai-agent -p nomifun-db`；`cargo clippy` 相关 crate 无新警告。

### Task 16: 多agent审查（需求7）
- [ ] code-review workflow：正确性/资源泄露/内存/状态机 多镜头 + 对抗验证本次全部 diff；重点：useRunLive timer 清理、strip 订阅、needs_review×(重试/取消/暂停/看门狗/adopt) 状态机交互、迁移兼容。修复全部 CONFIRMED 项后重跑验证。

### Task 17: 收尾
- [ ] 需求6评估结论写入最终汇报（spec §1 已含）；更新 memory；汇总交付说明。

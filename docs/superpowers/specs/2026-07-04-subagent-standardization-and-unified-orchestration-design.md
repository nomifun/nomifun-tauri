# Subagent 能力标配化 + 统一编排引擎设计

> 日期：2026-07-04 ｜ 状态：**待用户审阅**（brainstorming 产物，未开始实现）
> 主题：移除「智能编排」独立概念，把 subagent 能力做成所有会话标配；修好运行控制；补齐工作流可靠性与节点间共享上下文；协作模型选择器 + 自动模型路由；并把「骨架式 DAG」与「Claude Code 式动态主 agent」统一为一套引擎。

---

## 0. 决策快照（用户离席，按推荐默认落，审阅时可逐条推翻）

| # | 分叉 | 采用默认 | 备选 |
|---|---|---|---|
| A | 架构方向 | **两模式一引擎**：动态主 agent 默认标配，骨架式按需，可嵌套 | 骨架为主+动态为辅；全面转动态 |
| B | 移除范围 | **删桌面入口 + 基础提示词留常驻轻量提示**，伙伴 `smart_orchestration` 作为独立域保留 | 三入口全删；删入口不留提示 |
| C | 共享上下文 | **共享笔记文件 + run 黑板 + 摘要注入** | 只修注入；给 run 绑共享知识库 |
| D | 模型自动选 | **标签启发式 + 可覆盖**（激活现有确定性 Router + 复用已存在的描述驱动 LLM 路由） | LLM 路由器；本期只做选择器 |

**若无异议，此四项即为设计前提。** 任一项改动会影响对应章节（A→§1/§7，B→§2，C→§5，D→§6）。

---

## 1. 核心架构决策：两模式一引擎（回答「合并还是交叉」）

### 1.1 关键事实：两种架构在系统里都已潜在存在

现状经代码勘察确认（非推演）：

- **骨架式**已完整落地：`nomi_run_create` → 规划器 LLM（`plan.rs`）→ 静态 DAG → `orch_run_tasks`/`orch_run_task_deps` → 画布（`OrchestrationTopPanel`/`DagCanvas`）→ 用户可预配节点（`NodePreconfigPanel`，迁移 026 每节点模型覆盖）。
- **动态式**已具雏形：**lead 会话本身就是主 agent**——它已能随时调 `nomi_spawn`（扁平并行、无规划器）派发 subagent，run 到终态后 `LeadReporter.report`（`engine.rs:157`）以「一次性回执」重新唤起 lead 会话继续推理。这正是 Claude Code 的「主控随时派发、完成后接管」模式。

两者**共用同一套底座**：同一个 `RunEngine`、同一张画布、同一组网关原语（`nomi_spawn`/`nomi_run_create`/`nomi_run_status`/`nomi_run_result`/`nomi_run_adjust`），同一套 run/task 表与链接机制（`extra.orchestrator_run_id`）。

### 1.2 正解：不是引擎层二选一，而是「一个 run、两种作图方式、可嵌套」

**统一心智模型：每个会话最多一个长生命周期的 run；区别只在于「节点是谁、何时加进去的」。**

| 维度 | 动态模式（默认标配） | 骨架模式（按需规划） |
|---|---|---|
| 作图者 | lead LLM（边想边加节点） | 用户 + 规划器（提前铺满，可手工调节） |
| 节点何时出现 | 增量、随推理涌现 | 一次性、执行前 |
| 画布语义 | 实时可视化「已发生/进行中」 | 前瞻、可编辑、可预配 |
| 触发 | 复杂/可并行时 lead 自发派发 | 用户显式要「先规划再执行」或 lead 判断需要成体系分解 |
| 承载原语 | `nomi_spawn` + 新增 append/dispatch | `nomi_run_create`（规划器） |

**可组合/嵌套（用户直觉里的「交叉」）**：动态主 agent 可为某子目标铺一张骨架子图；骨架里的某节点也可以是个会再动态派发的小主 agent。技术上即「run 内节点可再向本 run 追加节点」+「受控的子 run 嵌套」。

**「长周期自动调整、持续实施」= 自主主循环**：让 lead 带目标 + 退出条件跑 `plan→dispatch→observe→adapt` 的持久循环、无需用户逐轮驱动。复用已有的 AutoWork 持久循环外壳 + boot-resume + sweeper（详见 §7）。

### 1.3 为何这样统一（而非保留两个概念）

- **零重造引擎**：运行时长 DAG 已可工作——loop 每轮重查 `list_ready_tasks`，`RunLocks`（`engine.rs:281`）保证「追加节点」与「终态判定」原子互斥；唯一挡路的是 `adjust` 现在要求「无节点在跑」，放宽即得「边跑边派发」。
- **保住你的独特价值**：骨架 + 用户调节是主流方案没有的，作为一等「规划模式」保留。
- **补上主流能力**：动态主 agent 成为默认，覆盖绝大多数会话。
- **只维护一套东西**：一个引擎、一张画布、一组原语、一套可靠性机制。

---

## 2. 移除「智能编排」概念，subagent 标配化

### 2.1 事实基础

「智能编排」**不是独立系统**，只给普通 `type:'nomi'` 会话加了两样东西：① 一段引导 lead 的提示词；② 一个主/协作模型范围通道（`extra.orchestrator_model_range`）。真正的 subagent 能力（网关工具 + 画布）**在每个本地可信桌面会话里本已标配**（`caps_orchestrator.rs` 注册的工具 `.deny_on(Surface::Remote)`；代码注释：「Tool availability is NOT granted here … only shapes the prompt」）。

三个入口（均可选、互不为门）：
1. 首页 composer 的「智能编排」开关（`ComposerEntryStrip.tsx` / `GuidPage.orchestrationMode`）——**主入口**；
2. 全局设置开关 `nomi.autoOrchestration`（`SystemModalContent.tsx`）；
3. 伙伴域 `smart_orchestration`（`LearnTab.tsx`，独立领域）。

### 2.2 变更

**删除（桌面会话域）**
- FE：`ComposerEntryStrip` 的 orchestrate 按钮；`GuidPage.orchestrationMode` 状态/handler；`GuidActionRow` orchestration 分支；`useGuidSend.ts` 的 orchestration 发送分支（181–233）；`SystemModalContent` 的 `nomi.autoOrchestration` Switch；`configKeys.ts` 的 `nomi.autoOrchestration`；相关 i18n（`guid.entry.orchestrate` / `conversation.orchestration.startTitle` / `settings.autoOrchestration*`）。
- 后端：`factory/nomi.rs` 的 `LEAD_ORCHESTRATOR_PROMPT`、`is_orchestration_lead`、`compose_lead_prompt`、`auto_orchestration` 读取块（187–197）、`PREF_AUTO_ORCHESTRATION`；`NomiBuildExtra.orchestrator_role`（若无其它读者）。

**保留不动（= 被保留的 subagent 能力本体）**
- `caps_orchestrator.rs` 全部网关工具；整个 `nomifun-orchestrator` crate；迁移 018 表；`link_orchestrator_run` + `extra.orchestrator_run_id`；会话原生画布（`OrchestrationProvider`/`OrchestrationTopPanel`/`ConversationContentSwitcher`/`PlanApprovalBanner`/`useConversationRun`）；`pages/orchestrator/*` 组件库；`engine_spawn_enabled` 路由。
- 伙伴 `smart_orchestration`（独立领域，**保留**，见决策 B）。

**新增：常驻轻量提示（关键，防能力空置）**
- 把一段**精简的 subagent 使用提示**并入基础 nomi system prompt（`factory/nomi.rs` 组装处），常驻、极短，语义类似：「遇到可并行的独立子任务或需要成体系分解的复杂任务时，可用 `nomi_spawn`/`nomi_run_create` 派发 subagent 并在画布可视化；简单问题直接回答。」
- 这替代原 `LEAD_ORCHESTRATOR_PROMPT` 的**可发现性**职责，但不制造「模式」概念——它对所有桌面 nomi 会话一视同仁。

**协作模型通道迁移（衔接 §6）**
- 原 `extra.orchestrator_model_range` 由首页 orchestration 分支写入；删该分支后，改由**会话内协作模型选择器**写入 + 更新（§6.1）。这样「主/协作模型」从「编排专属」变成「所有会话可用」。

**善后**
- 孤儿前门 `POST /api/orchestrator/runs/adhoc`（`create_adhoc_run`，交互审批路径）+ `ipcBridge.orchestrator.runs.createAdhoc`：**无 FE 调用者**。默认**保留为后端/程序化入口**（WebUI/未来外部调用可能用），标注 `@internal`，不在本期删（低风险、删了要回退成本高）。审阅可改为一并删除。

### 2.3 验收
- 任意普通桌面 nomi 会话中，模型在合适场景自发调 `nomi_spawn`，画布点亮、run 正常执行、终态回报 lead——**全程无「智能编排」字样**。
- 首页与设置不再出现「智能编排」入口；伙伴页 `smart_orchestration` 仍在。

---

## 3. 运行控制修复（「开始/暂停」失效）

### 3.1 根因（按钮其实全接通，非 stub）

`RunControls.tsx`（在 `OrchestrationTopPanel` 头部，非 `DagCanvas`）的 `approve/pause/resume/cancel` 全部端到端接通（→ `ipcBridge.orchestrator.runs.*` → `routes.rs` → `run_service.transition()` → 引擎 loop 自门控，且有回归测试 `engine.rs:6118`/`routes.rs:1733`）。「失效」的真实原因是：

1. **按状态互斥显示**：`approve` 仅 `awaiting_plan_approval`、`pause` 仅 `running`、`resume` 仅 `paused`、`cancel` 仅非终态。`planning`/未加载(`status===''`)/终态下**这三个都不渲染**，只剩重新规划/终止。
2. **pause 非破坏性**：只挡新派发，进行中的最多 `cap=4` 个继续跑完 → 图全在飞时点了「没反应」。
3. **画布面板可折叠**（localStorage `nomifun:orchestration-canvas-collapsed`）→ 整排控件被藏。
4. **压根没有**「重启一个已停/空闲 run」的按钮（run 在 `running` 时循环因崩溃退出后，只能靠 resume 的 `!is_running` 兜底或 boot-resume）。

### 3.2 变更

**① 状态自适应「主控」（always-present）**
- 头部固定一个语义清晰的主操作按钮，按 run 状态 morph，永不「整个消失」：
  - `awaiting_plan_approval` → **批准并开始**
  - `running` → **暂停**
  - `paused` → **继续**
  - `planning` → **规划中…**（禁用态 + spinner，附「重新规划」）
  - `completed`/`failed`/`cancelled` → **重跑**（见 ③）
  - `status===''`/加载中 → 骨架占位，不显示「无控件」
- 旁置**终止**（非终态时）。折叠态也保留一个迷你主控（不再把控件全藏）。

**② pause 立即可见生效**
- 点 pause 后立即把状态置 `paused`（乐观）+ 头部显示「已暂停 · N 个进行中，排空中」计数（读进行中 worker 数），消除「点了没反应」。
- 可选加「立即暂停（取消进行中）」次级操作：调 `cancel_in_flight_conversations` 但保 run 于 `paused`（供用户明确要「硬停」时用）。

**③ 「重跑/重启」入口**
- 终态 run 头部提供**重跑**：对整 run 走 `rerun` 语义（复用 `rerun_task` 的 re-activate terminal→running；或新增 `rerun_run` 批量重置失败/全部节点）。
- `running` 但循环已死（`!is_running`）时，主控提供**重新启动循环**（调 `engine.start`）。

**④ planning 卡死可视化**
- run 停在 `planning`（已知 planner-empty/fail-soft 会卡）时，头部明确显示「规划中/规划失败」+ 一键「重新规划」，不再是「按钮都不在」的死状态。

### 3.3 验收
- 每种 run 状态下，头部**总有**一个明确、可点、有反馈的主操作。
- pause 在「所有节点都在飞」时也有可见反馈（暂停态 + 排空计数）。
- 终态/卡死 run 可从画布一键重跑/重规划。

---

## 4. 工作流可靠性（fail 感知 / 重试 / 完工 / 完整交付）

### 4.1 现状与缺口

- **有真重试**（迁移 024）：`settle_failed_or_retry` 按 worker 会话的 `error.retryable` 标记分类，可重试则指数退避（base 5s、cap 60s、上限 `DEFAULT_MAX_WORKER_RETRIES=3`）回 `pending`，run 保持 `running`。
- **缺口 1（最痛）**：**超时/空回复被判不可重试 → 永久失败**（只有显式带 `error.retryable` 标记的错误才重试；30 分钟超时或空文本回复不带该标记，被当成永久失败——与文档措辞矛盾）。
- **缺口 2**：一个**必需节点失败 → 下游永远卡 `pending`（从不 skip）→ 其它分支排空后整 run 判 `failed`**。无部分交付、无中途上报 lead 修复（lead 只在终态后被唤起一次）。
- **缺口 3**：**完成是纯状态判定**（所有节点 `done||skipped`），无「目标达成/产物存在」质量门。裸 agent DAG 无任何验收。
- **缺口 4**：`output_files` 字段从不写 → 无结构化产物登记。

### 4.2 变更

**① 重试分类修正 + run 级预算**
- 扩展 `WorkerRunner::last_error_retryable`（`worker.rs`）+ `settle_failed_or_retry`（`engine.rs:1632`）：把**超时**、**空文本回复**纳入可重试类（各自独立退避、计入 `attempt`），但受**新增 run 级总重试预算**约束（避免宽扇出下每节点各退避 3 次拖垮 run）——run 级 `max_total_retries`（默认按节点数派生，可配）。
- 分类矩阵明确落文档：rate-limit/gateway/5xx/timeout/empty → retryable；auth/billing/bad-request/schema → 永久失败。

**② 失败 → 中途上报 lead 修复（新增 mid-run 观测点）**
- 在 `settle_task_outcome`（`engine.rs:1555`）里，节点**永久失败**时（预算耗尽/不可重试），复用 `deps.lead_reporter` 发一条**中途失败回执**给 lead 会话（不等终态）：附节点标题 + `last_error` + 下游被阻塞清单。
- lead（master agent）据此可当轮决策：`rerun_task`（换模型/改 spec）、`adjust`（改图）、或 `adopt_task_result`（采纳 stuck worker 现有产出）、或判定「跳过并继续」。
- 这把「失败」从「静默卡死到终态」变成「主 agent 可介入的事件」，是可靠性的核心杠杆。

**③ 部分交付（partial delivery）**
- 新增节点级 `on_fail` 策略（`fail_run` | `skip_and_continue`，默认由 kind/角色决定；规划器可标注）：
  - `skip_and_continue`：节点永久失败 → 复用现有 `skip_downstream`（`engine.rs:3386`）把传递性下游标 `skipped`，run 可走到 `completed_with_failures`（**新增终态语义**，区别于 `completed`/`failed`）。
  - `fail_run`：保持现状（关键节点失败即整 run 失败）。
- run 终态回执如实汇总：完成 N、失败 M、跳过 K + 各自摘要 → **总有部分产物交付**，不再「一个节点拖垮全部」。

**④ 完成质量门（复杂 run）**
- 规划器对**复杂/长 run** 默认在汇聚处插一个 `verify`/acceptance 节点（复用已有 verify 聚合器语义），对照目标校验产物；不通过则 `skip_downstream` 或触发 ②的修复回执。
- 简单 run（单/少节点）不强加，避免过度工程。

**⑤ 产物登记（衔接 §5）**
- 激活 `orch_run_tasks.output_files`：节点在共享工作目录写出的关键文件路径登记到该字段（结构化交接），run 终态汇总「产出文件清单」。

### 4.3 验收
- 注入一个必超时的节点：能自动重试（受预算约束），耗尽后触发 lead 修复回执，且下游按 `on_fail` 策略要么被修复要么被跳过，run 走到 `completed_with_failures` 并交付其它分支产物。
- 复杂 run 终态带 verify 结论 + 产出文件清单。

---

## 5. 节点间共享上下文（现状几乎零共享）

### 5.1 现状

- 每节点是**全新会话**（`worker.rs:run_restricted` 每 task `create` 一次），无共享会话/记忆/兄弟感知。
- 唯一通道：把**直接上游**节点的**最终文本**（`output_summary`）注入下游 system_prompt（`collect_upstream_outputs`→`compose_brief`）。**传递性祖先丢失**；注入**不截断**可能撑爆预算。
- 有个**每 run 共享工作目录**（`run.work_dir` → 每节点 `extra.workspace`），能共享磁盘文件，但要显式传 `work_dir`，否则每节点独立临时目录。
- `orch_workspaces.context`（JSON）与 `orch_run_tasks.output_files` 两个天生该放共享态的字段**从没写过（死字段）**。

### 5.2 变更（决策 C：共享笔记 + run 黑板 + 摘要注入）

**① 共享运行工作目录 + 共享笔记文件**
- run 创建时若未指定 `work_dir`，**自动分配一个 run 专属共享工作目录**（复用 `companion/workspaces/{seq}_{名}` 式约定 / 已有 workspace 基础设施），所有节点共享同一 cwd。
- 目录内维护一个人类可读的 **`RUN_NOTES.md`**（共享知识文件）：run 启动时写入「目标 + 计划概要」；节点可读、可**追加**「发现/结论/给下游的提示」。
- full-role 节点获得一个轻量 **`shared_notes` 读写工具**（append/read），把「共享 memory」变成 agent 可主动写的东西（对齐用户原话「在当前工作空间建立共享知识文件」）。

**② run 黑板（结构化）**
- 复活死字段 `orch_workspaces.context`（或 run 级等价列）作 **结构化黑板**：`WorkspaceService` 落地 read/write，存 run 级键值（如「已确定的技术选型」「共享约束」），供节点/聚合器读。
- 与 `RUN_NOTES.md` 分工：笔记文件 = 人类可读叙事 + agent 自由追加；黑板 = 机器可读结构化状态。

**③ 注入增强（修传递性丢失 + 截断）**
- 下游 brief 注入从「仅直接上游」扩为：**run 目标 + 计划概要（截断）+ 直接上游全文（截断到预算）+ 相关传递性祖先摘要（`SUMMARY_TASK_OUTPUT_LEN` 级截断）+ 共享笔记指针（告知节点可读 `RUN_NOTES.md`/黑板）**。
- 所有注入统一走截断（现直接上游注入无上限，需补 cap），防 system_prompt 预算爆掉。

**④ 与产物登记联动**
- 节点写入共享目录的关键文件 → 登记 `output_files`（§4.2⑤）→ 下游可按路径直接取，而非靠上游在文本里提一嘴。

### 5.3 验收
- A→B→C 链：C 能拿到 A 的关键结论（经共享笔记/传递摘要），不再只见 B。
- 两并行节点可经共享目录/黑板交换中间产物；run 终态 `RUN_NOTES.md` 是一份可读的过程档案。
- 长上游产物不再撑爆下游 prompt（有截断）。

---

## 6. 协作模型选择器 + 自动模型路由

### 6.1 协作模型选择器搬进每个会话（决策 D 的「选择器」部分）

**现状**：会话 composer（`NomiModelSelector` in `NomiSendBox.tsx:782`）**只有主模型选择器，无协作模型**；协作选择器只在首页 orchestration 模式下（`GuidCollaboratorSelector`）。`extra.orchestrator_model_range` **只在会话创建时写一次、无更新路径**。

**变更**：
- 在会话 composer 主模型选择器**旁**新增**协作模型选择器**（复用 `GuidCollaboratorSelector` 组件 + `useModelRange`/`useModelProviderList`；主模型钉为禁用的必选项 `· 主`，其余为协作池）。
- 新增**活跃会话更新 `extra.orchestrator_model_range` 的路径**（现只在 `create` 时写）：选择器变更即 `update_extra`（走 `orchestrator_model_range` 约定：`models[0]=主模型=lead/planner`，其余=协作池）。`caps_orchestrator.read_conversation_model_range` 已能读回，无需改后端读侧。
- 默认值：主模型 = 会话当前模型；协作池默认空（= 后端 `Auto` 兜底，即所有启用模型）。持久化沿用 `nomi.orchestrationCollaborators` 或改为会话级偏好。

### 6.2 自动模型路由（决策 D 的「自动选」部分）

**关键发现：描述驱动的 LLM 路由已经在跑，确定性能力路由是空转的。**

- **已工作**：规划器 `PLAN_SYSTEM`（`plan.rs:562`）已指示「按 desc/strengths 做难度↔性价比匹配、简单/批量用便宜快、难/推理重留强模型、首个成员=主模型」；`build_plan_user_prompt` 已把每模型的用户 `model_descriptions` 渲染进 `desc=` 列；规划器产出 `task_profile{needs_vision, needs_long_context, needs_high_reasoning, bulk}` + `member_index`。→ **用户要的「参考模型描述智能选」主路径已存在**。
- **空转**：确定性 Router（`router.rs::score_member`/`rank_members`）有 `needs_vision && !has_vision` 硬过滤，但 `has_vision` 读的 `capability_profile.modalities` 对会话原生成员**永远为空**（`infer_model_capability`/`derive_capability` 从不填 `"vision"`；`ProviderSummary` 丢掉了 `capabilities`/`model_context_limits`，只留 `model_descriptions`）。→ 视觉/长上下文的确定性过滤实际不生效。
- **无成本字段**：成本纯靠模型名硬猜（STRONG/LIGHT 列表）。

**变更（标签启发式 + 可覆盖，最小改动激活现有轨道）**：
1. **线程能力元数据进编排层**（cheapest，无新 schema）：`summarize_provider`/`ProviderSummary`（`tools_provider.rs`）补带 `capabilities`（`ModelType`）+ `model_context_limits`；`build_members_from_range`/`build_assistant_members` 据此填 `CapabilityProfile.modalities`（含 `"vision"`，可结合已有的模型名启发式端口到 Rust）与 reasoning/cost tier。→ **让 `router.rs` 的 `needs_vision`/`needs_long_context` 硬过滤真正生效**，图片理解节点自动排除非视觉模型、落到视觉模型。
2. **可覆盖**：保留每节点手动覆盖（迁移 026，`NodePreconfigPanel`）为最高优先；确定性 Router 为自动兜底；描述驱动 LLM 路由为主选。三层优先级：**手动覆盖 > 规划器 LLM 选择（存活于 Router 硬过滤）> Router 兜底 top pick**（沿用现有 `resolve_assignment_pick` 语义）。
3. **（可选，审阅决定）** 若要用户权威的每模型能力/成本标签：仿 `model_descriptions` 的 `model_id→value` JSON 模式加 `model_capability_tags`（迁移）+ hub 编辑器（`ModelModalContent.tsx` 里 `ModelDescriptionEditor` 旁）。本期默认**不做**，先用启发式 + 描述；作为后续增强。

### 6.3 验收
- 每个会话 composer 主模型旁有协作模型选择器；改动即时反映到该会话后续派发的模型池，主模型即 lead/planner。
- 明确带图片理解需求的节点：确定性 Router 自动排除非视觉模型（`needs_vision` 硬过滤生效）；简单/批量节点倾向便宜模型（规划器描述驱动 + cost tier soft score）。
- 用户在 hub 写的模型描述真实影响自动选型（现已生效，回归验证不被本次改动破坏）。

---

## 7. 动态主 agent 模式 + 自主主循环 + 嵌套（架构 A 的落地）

### 7.1 统一数据模型：每会话一个长生命周期 run

- 一个 master 会话对应**一个持久 run**，节点可增量追加（动态）或一次性铺满（骨架）。画布始终展示这一个 run 的图。
- 现状 `nomi_spawn`/`nomi_run_create` 每次调用都 `create_adhoc` + `link`（可能造成一个会话多 run/重复 link）。**收敛为**：会话已有 `orchestrator_run_id` 时，后续派发**追加到现有 run**，而非新建。

### 7.2 边跑边派发（runtime DAG growth）

**已具备的安全底座**：loop 每轮重查 `list_ready_tasks`（新 `pending` 且依赖满足即被拾取）；`RunLocks::for_run` 保证追加与终态判定原子互斥；loop 在锁内注销 handle 使 `is_running()` 与终态写原子。

**变更**：
- 新增轻量 **`add_tasks(run_id, tasks)`** 控制面原语：复用 `plan_flat`/`reconcile_run_plan` 的插入逻辑，把 caller 指定的节点（无规划器）追加到**现有 run**，走同一 `assign_task` 路由，re-activate（若 run 已终态）或让存活 loop 下轮自取。
- **放宽 `adjust`/append 的「无节点在跑」限制**（`run_service.rs:1461/1516`）：允许在 worker 运行时追加节点（`RunLocks` 已保证安全）。这是「边跑边派发」的唯一阻塞点。
- 网关暴露一个供 lead 使用的「向本 run 追加子任务」工具（区别于新建 run 的 `nomi_spawn`）。

### 7.3 动态主 agent 的中途观测

**现状**：`LeadReporter` 只在**终态**唤起 lead 一次；中途进度只经 WS 发 FE，不回 lead 会话。

**变更**：
- 增加**中途观测回执**（在 `settle_task_outcome` 后 / 每 fill 批次后，节流）：把「阶段性完成/失败」以 lead 会话轮的形式回报，使 master 能「观测→再派发」形成 Claude Code 式循环。
- 节流与去噪：避免每节点都唤起 lead（成本）；按「批次完成」或「关键节点/失败」触发。

### 7.4 自主主循环（长周期、无用户逐轮）

**复用**：AutoWork（`nomifun-requirement/src/orchestrator.rs`）的持久循环外壳——`claim→inject→wait→finalize→idle-on-wake→repeat` + `resume_persisted_bindings`（boot-resume）+ `start_sweeper`（60s 收租）+ `failure_backoff`。引擎注释自承「RunEngine 是 AutoWork 的忠实缩减版」。

**变更**：
- 让 `autonomous` 自主模式**真正生效**（现仅为 `supervised` 同义词）：master run 带**目标 + 退出条件**，跑 `plan→dispatch→observe→adapt` 持久循环，无需用户逐轮驱动；到达退出条件（目标达成/预算耗尽/连续 N 轮无进展/用户中止）才终止。
- 复用 boot-resume（`resume_persisted_runs`）+ 看门狗（`reap_stalled_runs`）+ `RunLocks`：持久 master run 跨重启自动续跑，有 liveness 兜底。
- 与 §4 可靠性联动：自主循环遇节点失败走「重试→中途修复回执→部分交付」而非停摆。

### 7.5 受控嵌套子委派

**现状不安全**：worker 的 `orchestrator_run_id`/`orchestrator_task_id` 仅关联标记、无人消费；worker 调 `nomi_run_create` 会建**孤儿子 run**（挂在一次性 worker 会话上，其终态回执落到已结束的会话）；**无深度守卫**（嵌套无界、未测）。

**变更**：
- 加 **委派深度字段 + 上限守卫**（worker `extra` 传递并消费 depth，超限拒绝再派发）。
- 子 run 要么**嫁接进父 run 的图**（受控 append，父 run 统一可视化），要么建**受管子 run** 且其终态有**活跃重唤起路径**（不落到已结束的一次性会话）。
- 只读受限角色（`searcher/scout/reviewer/verifier/tester`，`desktopGateway=false`）**继续禁派发**（防只读越权）；仅 full-role 且深度未超限可再委派。

### 7.6 验收
- 一个会话内，主 agent 可多轮 dispatch→observe→dispatch，画布是同一个持续生长的 run。
- 开启自主模式给定目标 + 退出条件，master 无人值守跑到目标达成/退出条件，跨重启续跑。
- 嵌套受深度约束、无孤儿子 run、只读角色不越权。

---

## 8. 数据模型 / 迁移汇总

| 迁移 | 内容 | 归属 |
|---|---|---|
| 复活 `orch_workspaces.context`（读写落地，非新迁移） | run 结构化黑板 | §5 |
| 激活 `orch_run_tasks.output_files`（非新迁移，改写入逻辑） | 节点产物登记 | §4/§5 |
| 新增 run 级 `max_total_retries` / `completed_with_failures` 状态语义 | 可靠性 | §4 |
| 新增节点 `on_fail` 策略列 | 部分交付 | §4 |
| 新增 run 委派 `depth` 字段（或复用 task 字段） | 嵌套守卫 | §7 |
| （可选）`providers.model_capability_tags`（仿 `model_descriptions`） | 每模型能力标签 | §6（默认不做） |

> 迁移号按落地时的最新号顺延；**每加迁移必 bump `db_lifecycle` pre_baseline**（既往教训）。避免与并行端撞号（提交前 `git pull --rebase`）。

---

## 9. 分期实施路线（每期各自 → writing-plans）

> 本 spec 覆盖多子系统，**不作为单个实现计划**，按下列顺序拆成独立计划分批交付。

**Phase 0 — 标配化 + 控制修复（低风险、先落、独立可交付）**
- W1 移除「智能编排」入口 + 常驻提示（§2）
- W2 运行控制修复（§3）
- W6a 协作模型选择器进会话 composer + 更新路径（§6.1）

**Phase 1 — 可靠性 + 共享上下文（核心价值）**
- W4 重试分类/预算 + 失败中途修复回执 + 部分交付 + 完成质量门（§4）
- W5 共享工作目录 + `RUN_NOTES.md` + run 黑板 + 注入增强（§5）

**Phase 2 — 模型路由激活**
- W6b 线程能力元数据进编排层、激活确定性 Router 硬过滤（§6.2）

**Phase 3 — 动态主 agent + 自主循环 + 嵌套（架构收官）**
- W7a 单会话单持久 run + `add_tasks` + 放宽 running-append（§7.1–7.2）
- W7b 中途观测回执 + 动态 master 提示（§7.3）
- W7c 自主主循环（`autonomous` 真化，嫁接 AutoWork 外壳）（§7.4）
- W7d 受控嵌套子委派（§7.5）

每期结束跑触碰 crate 的 `nextest` + `bun run typecheck`（exit 0）；收尾全量。UI 改动过 frontend-design 且对齐既有视觉语言。

---

## 10. 风险与未决

1. **两套重试/自愈相撞**：worker 是 `session_mode=yolo` 真会话，是否被 IDMM `ConversationSupervisionHook` 监管？若是，IDMM sidecar 备份模型可能在编排器 `last_error_retryable` 看到终态标记前就在会话内自愈——两系统交互需明确归属（IDMM 会话内愈 vs 编排器整节点重试），或显式把编排 worker 排除出 IDMM。**实现前须确认 `nomifun-app/router/state.rs` 的挂载范围。**
2. **看门狗调度确认**：`reap_stalled_runs` 依赖 app 周期调用（~60s，`state.rs:1227`）——自主循环上线前须确认其确实在跑。
3. **中途观测的成本/噪声**：mid-run 回执唤起 lead 会话有 token 成本，节流策略要实测调参。
4. **runtime append 与完成判定竞态**：放宽「running 时 append」后，须确保追加节点在 `RunLocks` 内与 `finish_run` 原子互斥不 strand（现有锁语义支持，但要专门测）。
5. **provider 级 vs 每模型能力**：`ModelType` 是 provider 级，一个 provider 多模型视觉能力不一；线程进编排层时以「provider 能力 ∩ 模型名启发式」近似，边界情形可能误判——必要时回退到「可选每模型标签」（§6.2③）。
6. **单会话单 run 收敛的回归面**：现允许一会话多 run/重复 link；收敛为单持久 run 要排查所有依赖「每次调用新建 run」的路径（含孤儿前门、伙伴域）。
7. **`completed_with_failures` 新终态**：FE 状态元（`runStatusMeta.ts`）、lead 回执、看门狗、boot-resume 都要认这个新状态。

---

## 附：关键符号索引（实现锚点）

- 引擎：`RunEngine`/`run_loop`/`dispatch_task`/`settle_task_outcome`/`settle_failed_or_retry`/`mark_task_failed`/`finish_run`/`reap_stalled_runs`/`RunLocks`（`nomifun-orchestrator/src/engine.rs`）
- 控制面：`create_adhoc`/`spawn_plan_and_start`/`spawn_plan_flat_and_start`/`plan`/`plan_flat`/`replan`/`compute_adjusted_plan`/`apply_adjusted_plan`/`rerun_task`/`adopt_task_result`/`transition`/`approve_plan`/`pause`/`resume`/`resolve_assignment_pick`/`build_members_from_range`/`infer_model_capability`（`run_service.rs`）
- 规划/路由：`plan.rs`（`PLAN_SYSTEM`/`pick_lead`/`build_plan_user_prompt`/`build_description_map`）、`router.rs`（`score_member`/`rank_members`）
- worker：`run_restricted`/`build_worker_extra`/`role_allowed_tools`/`read_final_text`/`last_error_retryable`（`worker.rs`）
- 读模型：`list_ready_tasks`/`reset_orphaned_running_tasks`/`reconcile_run_plan`（`sqlite_orch_run.rs`）
- 网关：`caps_orchestrator.rs`（`nomi_spawn`/`nomi_run_create`/`read_conversation_model_range`/`expand_auto_range`/`build_adhoc_request`）、`tools_provider.rs`（`summarize_provider`/`ProviderSummary`）
- 工厂：`factory/nomi.rs`（`LEAD_ORCHESTRATOR_PROMPT`/`is_orchestration_lead`/`compose_lead_prompt`/`engine_spawn_enabled`）
- 链接：`link_orchestrator_run`/`apply_knowledge_mounts`/`session_workpath_key`（`nomifun-conversation/src/service.rs`、`nomifun-knowledge/src/workpath.rs`）
- 自主循环：`nomifun-requirement/src/orchestrator.rs`（`run_loop`/`resume_persisted_bindings`/`start_sweeper`/`failure_backoff`）
- 模型元数据：迁移 `021_model_descriptions.sql`/`025_model_context_limits.sql`/`026`（每节点覆盖）；`ModelType`/`ModelCapability`/`ProviderResponse`（`nomifun-api-types/src/provider.rs`）；`ModelDescriptionEditor`（`ModelModalContent.tsx`）
- FE：`NomiModelSelector`/`NomiSendBox`、`GuidCollaboratorSelector`/`useModelRange`/`useGuidSend`、`OrchestrationTopPanel`/`RunControls`/`DagCanvas`/`NodePreconfigPanel`/`useConversationRun`/`PlanApprovalBanner`

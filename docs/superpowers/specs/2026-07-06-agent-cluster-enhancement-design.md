# Agent 集群（多 agent 协作）特化增强 — 设计文档

日期：2026-07-06
状态：自主模式下定稿（用户以 /goal 下达 7 点需求，本文档为方案权衡与设计决策记录）

## 0. 背景与目标

编排能力已经历三代演进（独立 Tab → 会话原生化 → subagent 标配化 + 分层自愈），当前架构：
lead agent 通过 `nomi_run_create`（规划 DAG）/`nomi_spawn`（扁平扇出）铸造 run，每节点 = 一条真实
nomi worker 会话，前端经 `conversation.extra.orchestrator_run_id` 被动感知并展示右侧画布。

问题（用户 7 点）：
1. 用户完全感知不到多 agent 编排能力（Phase 0 删光了所有显式入口）；
2. 画布 UI 糙、无设计感（节点样式、点击交互）；
3. 画布渲染慢，且规划过程无跟踪反馈；
4. 主 agent 不实时反馈各节点交付，阶段环节无感、体验断联；
5. 画布上的 main agent 节点概念奇怪（它贯穿全程、非首节点）；缺「审批模式」——节点决策问题应可由用户亲自进节点作答；
6. 「各节点总结 skill/助手」疑似消失，需评估是否加回；
7. 完成后全局审视代码风险/内存/资源泄露。

## 1. 需求 6 评估结论（先说清，因为它影响范围）

**结论：不存在"被移除的各节点总结 skill/助手"，不建议以独立 skill 形式新增 per-node LLM 总结。**

依据（git 全历史 + 代码核查）：
- `git log -S "节点总结"` 零命中；summarize 相关命中全部是 **run 级** LLM 综合总结（509d22e 引入
  `compute_completed_summary`，失败 fail-soft 回退 `aggregate_summary`），至今活跃未删。
- 节点的 `output_summary` 就是该节点 agent 自己的最终文本（engine.rs settle 直接写 `o.text`），
  并已通过 Phase 1b 注入下游节点 brief（目标+计划+祖先产出+笔记指针）。
- `synthesis` 任务类型（综合上游产出）也仍在。

用户感知到的"总结效果消失"，真因是**反馈链路缺口**：节点完成只发画布 WS 事件，不进会话；
批量回执被 ≥3 节点 && ≥20s 双重节流，小型 run 常常直到终态才有一条会话反馈。
**处置：不加 per-node 总结 skill（run 内多一次 LLM 调用 = 加延迟加成本，价值已被 brief 注入/
run 级总结/ synthesis 覆盖），改为把既有 per-node `output_summary` 真正送到用户眼前**（见 §4、§5）。

## 2. 需求 1 — 「agent 集群」入口回归

### 方案权衡
- A. 恢复旧「智能编排模式」硬分支（前端建 lead 会话 + orchestrator_role）——被 07-04 spec 明确
  否决过（伪模式、制造割裂），不走回头路。
- B. 后端硬门（点了按钮就必开 run）——违背"简单任务直接答"的用户自己的要求，且规划器对
  单步任务产出退化 DAG，浪费。
- **C（采用）. 意图标记 + 提示升级**：按钮 = 写 `conversation.extra.agent_cluster_mode: true`；
  后端 `factory/nomi.rs` 检测该标记，在 `SUBAGENT_STANDARD_HINT` 之上追加更强的
  `CLUSTER_MODE_HINT`：**必须刻意评估**是否开集群（nomi_run_create/nomi_spawn），若判定太简单，
  **必须在回复开头向用户说明使用简单模式的原因**再直接作答。

与现架构一致（编排=标配能力，模型决策），同时满足"不管什么难度都要刻意判断 + 太简单要说明原因"。

### 落点
- **首页（guid）**：`ComposerEntryStrip` 默认态最左（「召唤助手」左边）新增「agent 集群」toggle
  按钮（复用 `entryButton`/`entryButtonActive` 样式，icon-park 图标不用 as 别名）。选中后
  `useGuidSend` 创建会话时写 `extra.agent_cluster_mode=true`。
- **会话页**：composer rightTools 新增集群 pill（与 collaboratorSelectorNode 同位注入），
  显示/切换本会话集群模式，写回 `conversation.extra`（浅合并，仅覆盖本键）。popover 内同时
  承载**审批模式**开关（§6）。
- i18n：zh-CN + en-US 同步新增 `guid.entry.cluster` / `conversation.cluster.*` 等 key，重新生成
  i18n-keys.d.ts。

## 3. 需求 2 — 画布 UI 精美化

保持"主题变量 only、无硬编码 hex"约束与既有节点签名缓存机制，重构视觉层：

- **TaskNode 卡片**：分层结构（顶部状态条纹→标题区→meta 区）、精细阴影梯度（rest/hover/active
  三态）、hover 抬升 + 边框亮化、按压回弹（scale 0.98）、running 态柔和呼吸光晕 + 顶部进度
  shimmer、selected 态动画光环（脉冲 ring）、状态色沿用 `taskStatusMeta` 单一真源。
- **needs_review 决策态**：琥珀色 + 醒目提问徽标（Help 图标 + 缓脉冲），一眼可见（§6 联动）。
- **边**：默认淡雅曲线；下游 running 时渐变流光（CSS animation on dash）；main 边随 main 节点
  移除一并消失。
- **入场动画**：planUpdated 后节点错峰淡入上浮（stagger 40ms），消除"憋一口气全量闪现"的糙感，
  同时显著改善"渲染慢"的体感（§4 配合真性能修复）。
- **画布 chrome**：Controls/MiniMap 圆角阴影微调、背景点阵密度降噪。
- 交互样式全部走 `dag-canvas.css` 类（:hover/:active 无法内联），节点内只留必须的动态内联色。

## 4. 需求 3 — 渲染性能 + 规划过程反馈

真性能修复（证据均已定位）：
1. **单一数据源**：`DagCanvas` 删掉自己的 `useRunLive`，改由 props 接收 `detail/loading/refetch`
  （唯一消费者是 `OrchestrationTopPanel`，从 context 取）。消除同 run 双订阅双 refetch。
2. **refetch 去抖合并**：`useRunLive` 事件驱动 refetch 加 trailing 去抖（~180ms）+ 在飞合并
   （fetch 进行中再来事件只标记 dirty，返回后补一次），突发事件从 N 次 REST 降到 1-2 次。
3. **廉价签名**：节点对象复用签名从 `JSON.stringify(built)` 换成手工拼接的渲染相关字段串
   （status/selected/title/pill 数据/position…），每渲染每节点 O(字段数)。
4. **缓存清理**：`nodeCacheRef` 每次构建后剔除已不在 tasks 里的 id（长会话内存泄露点，§8 复核）。

规划过程反馈（体感）：
- 画布"规划中"占位升级为**实时规划叙事**：消费 `useLeadThinking` 的 `phaseKeys`（planning_started/
  decomposing/assigning…）逐条点亮 + reasoning 摘要滚动，用户能看见"设计流程"在推进；
- 会话侧由集群进度条（§5）同步显示规划阶段。

## 5. 需求 4 — 主 agent 实时反馈各节点交付

**双层设计：UI 实时层（0 LLM 成本、WS 驱动、必达）+ lead 叙事层（LLM 回执，节流放宽）。**

否决项：逐节点 LeadReporter 回执（每节点 = steer 主 agent 一轮 LLM = 刷屏 + 成本 + 与自主编排
叠加放大），保留节流是既有设计的正确部分。

- **UI 实时层（新组件 ClusterProgressStrip）**：挂在会话内容区顶部（PlanApprovalBanner 同级），
  run 存在即显示：规划阶段叙事 → 每节点 chip（状态色点 + 标题 + attempt/tokens 摘要 +
  needs_review 提问徽标），WS 事件实时驱动；点击 chip 直接 `projectTask` 投影进该节点。
  这是"每个阶段环节都有反馈 + 不可能断联"的保障层——不经过任何模型。
- **lead 叙事层**：`BATCH_REPORT_MIN_NODES` 3→1（保留 ≥20s 间隔节流）：中途每有节点交付，
  最迟 20s 内主 agent 收到一次批量回执（含 `build_summary_digest` 的 per-node 产出摘要）并向
  用户转述——即"由主 agent 代理转达各节点产出"（需求 5 前半）。终态/卡死/单节点永久失败回执
  与 exactly-once/best-effort 不变量全部保持。
- 更新既有 `batch_progress_reports_to_lead_midrun` 单测阈值断言。

## 6. 需求 5 — 移除 main 节点 + 节点级审批模式

### 6.1 移除画布 main 节点
`OrchestrationTopPanel` 不再传 `onOpenMain/mainActive`，并**删除** DagCanvas 的 main 注入路径 +
`MainNode.tsx`（该路径唯一消费者就是这里，留着即死代码）：删 `MAIN_NODE_ID/MAIN_ROW_OFFSET/
computeRootTaskIds 的 main 用途/main 边/NODE_TYPES_WITH_MAIN`。「回到主会话」已由
ProjectedWorkerView 的「← 返回 main」承载，不受影响。layout 不再整体下移一行。

### 6.2 节点级审批模式（即 Phase D 缓议的 needs_review 逐任务门，正式落地）
概念：**全授权（auto，默认）** = 节点遇抉择自行判断（brief 指令）；**审批模式（manual）** =
节点遇重大决策问题可挂起向用户提问。

数据与状态：
- 迁移 028：`orch_runs.approval_mode TEXT NOT NULL DEFAULT 'auto'`；
  `orch_run_tasks.pending_question TEXT`。
- 设置真源：`conversation.extra.orchestrator_approval_mode`（会话页集群 pill popover 切换）；
  `nomi_run_create`/`nomi_spawn` 建 run 时读取写入 run 行（与 model_range 同法）。
- 复用既有 task 状态字面量 `needs_review`（前端 taskStatusMeta/MiniMap 色已备好）作为"节点挂起
  待人判"状态。

提问链路（仅 approval_mode='manual' 时启用）：
1. worker brief 追加指令：遇显著决策分歧时调用 `nomi_task_question(question)` 后结束本轮等待；
   auto 模式则明确"自行选择最合理方案并在产出中说明"。
2. 新网关工具 `nomi_task_question`：仅 worker 会话（extra 携 orchestrator_task_id）可调；写
   `pending_question`、置 task `needs_review`、emit `task.statusChanged`、
   `LeadReporter.report(NodeQuestion{title, question})`。
3. 引擎 settle：worker 回合结束时若 task 已是 `needs_review` → 不落 done、不激活下游、不计失败；
   run 保持 running。看门狗豁免：存在 needs_review 任务的 run 不判 stalled（合法等人）。
4. 主 agent 收到 NodeQuestion 回执 → 向用户转述"节点 X 有决策问题，请进入该节点作答"；画布该
   节点亮提问徽标；ClusterProgressStrip 同步显示提问横幅（点击直达）。
5. 用户深入节点（ProjectedWorkerView，顶部展示 pending_question 横幅）→ 在 worker 会话内直接
   回答（既有完整 composer）→ worker 继续产出 → 用户点既有「采用为该节点产出」：adopt 清
   `pending_question`、置 done、重激活 run（完全复用 UC-2c 路径）。

不变量守护：回执 best-effort；per-run 锁不跨 LLM await；`nomi_task_question` 不持终态锁；
Remote 面继续硬拒。

## 7. 交付物与验证

- 前端：`bun run build` + `check:i18n`（zh-CN/en-US 全 key）+ ComposerEntryStrip 单测更新。
- 后端：`cargo test -p nomifun-orchestrator -p nomifun-gateway -p nomifun-app`；新增/更新单测：
  compose_lead_receipt NodeQuestion arm、BatchProgress 阈值、needs_review park settle、
  approval_mode 建 run 透传。
- 全局审查（需求 7）：多 agent code-review workflow（正确性/资源泄露/内存 双镜头 + 对抗验证）
  覆盖本次 diff；重点核查：订阅/Observer/timer 清理、nodeCacheRef 清理、去抖 timer 卸载清理、
  needs_review 与看门狗/重试/取消的状态机交互。

## 8. 明确不做（YAGNI）

- 不恢复独立编排 Tab / 悬浮画布 / 全局 autoOrchestration 开关；
- 不做 per-node LLM 总结 skill（§1 结论）；
- 不做逐节点 lead 回执（用 UI 实时层替代）；
- 不改 worker 引擎调度/重试/自愈主体逻辑（Phase A/B/C 成果原样保留）；
- 伙伴（companion）表面零改动（Provider 缺省直通路径保持）。

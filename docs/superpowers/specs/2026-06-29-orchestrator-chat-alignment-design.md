# 智能编排页对齐会话页 UI/交互 + 主 Agent 编排思考流 · 设计

> 状态:已与用户确认设计方向(全面对齐 / 推理优先+阶段叙述)。本文件是已批准设计的记录,实施计划见
> `docs/superpowers/plans/2026-06-29-orchestrator-chat-alignment.md`。

## 背景与诉求

用户两条诉求(原话):
1. 「这个页面的 UI 还是太丑了,能否对齐会话页面的 UI 设计和交互?」
2. 「输入会话内容提交后,有很长的空挡时间等待,完全没有看到模型对于编排的思考过程,这个体验也非常不好,请优化。」

参照面 = 会话页(工作台首页 `GuidPage` 的输入卡 + 会话内 `ChatLayout`/`SendBox` 的视觉语言)。

## 调查结论(理解阶段产出)

### 会话页视觉语言来源
- 工作台首页输入卡 `GuidInputCard`:外层 `--bg-2` + 内层 **rd-24px 白卡**(`--bg-base` / 边框 `--color-border-3`),
  顶部标签条 `ComposerEntryStrip`,底部工具栏 `GuidActionRow`(模型 pill `GuidModelSelector` +
  权限 pill `AgentModeSelector` + **圆形发送钮**)。容器 `.guidLayout` = `clamp(360px, 100%-32px, 800px)` 居中。
- 焦点淡紫辉光 = `ui/src/renderer/hooks/chat/useInputFocusRing.ts`(**零业务依赖,可直接复用**;返回
  `activeBorderColor`/`inactiveBorderColor`/`activeShadow`)。
- 会话内 `components/chat/SendBox`(rd-20px、`.send-button-custom` 圆形钮、`.sendbox-model-btn` pill)——
  但 **强耦合 `ConversationContext`/`PreviewContext`,不可直接搬**,只能借类名 token。
- `ChatLayout` 是会话页壳(玻璃头 `chat-layout-header--glass` + 内容 + 工作区右栏 + 预览分栏);自挂
  `PreviewProvider`,以 conversation_id 驱动。

### 编排页现状(待改)
- master-detail:300px `RunListRail` + detail(三态:`NewRunIntentBox` 空态 / `NewRunComposer` 表单 / `RunView`)。
- `NewRunIntentBox` / `RunIntentBox` 是**手搓扁平 rd-14px 卡 + 32px 方形 div 发送钮**(非 SendBox),列宽 560/720。
- `RunView` 仅一条细边切换条(对话⟷编排画布,localStorage `nomifun:orchestrator-runview-mode` 默认 conversation);
  **无玻璃头**;运行控制(取消/批准/暂停/恢复)埋在 `DagCanvas`/`RunDetailHeader` 内。
- 对话视图 = `RunDecisionFeed`:**纯前端**由 `TRunDetail` 重建的 chat 风气泡线(已最接近会话,但无实时 LLM 文本)。

### 「空挡」真因
- 提交后 HTTP 处理器**同步**跑完整段 lead 规划:`create_adhoc_run` → `RunService::plan` → `planner.produce` →
  `one_shot_completion(cfg, PLAN_SYSTEM, …, 4096)`(单次 ~4096 token 阻塞结构化补全),**在路由返回前**完成;
  `adjust` 同理。规划**不在 run_loop 内**,run_loop 只在计划落库后调度。
- 但 `one_shot_completion == streaming_completion(cfg, system, msgs, max, |_| {})`——底层 **一直在流式产出**
  `LlmEvent::{TextDelta, ThinkingDelta, ThinkingSignature, …}`,只是被空回调丢弃。`streaming_completion` 的
  `on_delta` 注释明言「便于调用方把增量 fan out 到 WebSocket」。
- 流式承载与渲染原语均现成:WS 由单一 `EventBroadcaster` 总线按事件名多路复用;`AgentStreamEvent::Thinking`
  → 前端 `MessageThinking` 折叠气泡。编排现有 5 个 WS 事件(`orchestrator.run.statusChanged` / `planUpdated` /
  `completed`、`task.statusChanged` / `assigned`)无 token/思考事件。

### 关键不变量(必须守)
- **per-run 锁(`RunLocks`)绝不可跨 LLM await**。CREATE/PLAN 路径在 `engine.start` 前规划,**不持锁**(可自由流式);
  ADJUST 路径当前 `engine.adjust` 持锁后才调 `planner.adjust`(锁内跨 await)——流式前**必须把 lead 调用移出锁**
  (锁外算 `AdjustedPlan`,锁内只做 `reconcile_run_plan` + 重激活),对齐 summarize 既有做法。
- 编排 WS 事件是**手镜像**(`ui/src/common/types/orchestrator/orchestratorEvents.ts` + ipcBridge,无 codegen)——
  新事件须两端同步加。
- 无 IR/编译/节点图(已撤回方向,禁复活);禁 cargo fmt;禁合并 main;提交前 git pull --rebase;
  icon-park 具名禁别名;`<div role=button>` 非裸 `<button>`;Arco 弹窗经 useArcoMessage;无 any/ts-ignore;
  主题色一律走 CSS 变量,不硬编码(护 5 套主题);用 `npm run typecheck`。

## 已批准设计

### A. UI 对齐(力度:全面对齐)
1. **意图 composer(共用)**:新建一个编排专用、chat 风格的 composer 组件,替换 `NewRunIntentBox` 与
   `RunIntentBox` 的输入部分。
   - 外观:**rd-24px 双层卡**(外 `--bg-2`/内 `--bg-base`,边框 `--color-border-3`),复用 `useInputFocusRing` 辉光,
     **圆形发送钮**(复用 `.send-button-custom`,ArrowUp/Send 白色填充),**800px 居中**。
   - 工具栏:把**模型范围**(auto/single/range)与**自主度**(interactive/supervised)做成工具栏 **pill**
     (摆位/类名对齐 `GuidActionRow` 的模型/权限 pill);模型范围 pill 点开 popover 选 auto/single/range,保留多模型语义。
   - 会话页的「自由发挥/召唤助手/Skills」标签条**不照搬**(对编排无语义),由上述编排 pill 取代。
   - **不**耦合 `ConversationContext`/`PreviewContext`——自建组件、只借类名/hook。
2. **RunView 玻璃头**:加会话同款 `chat-layout-header--glass` 观感头部:运行目标(行内重命名)+ 状态 pill +
   运行控制(取消/批准/暂停/恢复,**从 DagCanvas 上提**)+ 对话/画布切换作为 `headerExtra`。两视图共用。
3. **决策流 / 列表对齐**:`RunDecisionFeed` 气泡与任务行、`RunListRail` 行,统一到会话观感(头像+标题+副标题+
   hover 操作、圆角/间距/主题 token)。
4. **列宽**:意图卡与详情列统一到 800px。

### B. 思考流(内容:推理优先 + 阶段叙述)
1. **后端事件**:`OrchestratorRunEventEmitter` 新增 `emit_lead_thinking`,广播 WS
   `orchestrator.run.leadThinking`,payload:
   `{ run_id, phase: "plan"|"adjust"|"summarize", kind: "reasoning"|"text"|"phase", delta?: string, content?: string, done?: bool }`。
2. **流式源**:在 `nomifun-ai-agent` provider_config 增加可同时转发 `TextDelta` 与 `ThinkingDelta`(带 kind 区分)的
   流式入口(现 `drain_text_response_with` 只转发 TextDelta);`PlanProducer`/`RunService` 接入一个 lead-thinking sink,
   把 produce/adjust/summarize 的空回调换成真转发。后端按 N ms / M 字符**合并**增量,防 WS 洪泛。
   - `kind:"reasoning"` = `ThinkingDelta`(可读推理,provider 支持时);`kind:"text"` = 计划 JSON 草稿
     (作进度心跳,前端**不裸显** JSON);`kind:"phase"` = 阶段叙述(见下)。provider 不支持推理时退化为阶段叙述 + 计划浮现。
3. **阶段叙述**:plan()/adjust() 在关键节点发 `kind:"phase"` 事件(正在拆解目标 → 分派 agent → 生成计划),
   保证任何 provider 都有确定性可见进度。
4. **消空挡(乐观创建)**:拆分 `create_adhoc_run` 路由,**立即返回 planning 态 run**;`plan()` + `engine.start` 在
   后台 spawn 跑、流式灌入。前端创建后**即时跳转** `?run=id`,落到 planning 态 RunView 看主 agent 思考拆解,
   而非卡在表单。FE 已在 `planUpdated` 重抓 `TRunDetail`,且能处理零任务 planning 态。
5. **adjust 流式**:按不变量先把 `planner.adjust` 移出 per-run 锁,再锁外流式。

### C. 前端消费
- 线契约两端同步加 `orchestrator.run.leadThinking`。
- 新增 `useLeadThinking(runId)`:**独立订阅**(不触发 `useRunLive` 的整体详情重抓),按 phase 累积
  reasoning/text 增量 + done,前端再合并节流。
- `RunDecisionFeed` 渲染流式「编排思考」气泡(复用 thinking 气泡观感),与决策卡共存:思考气泡在上(流式),
  编排决策卡在下(由 `TRunDetail` 实时派生的汇总)。

## 风险与对策
- **WS 洪泛**:后端合并 + 前端节流;`leadThinking` 订阅与 `useRunLive` 重抓解耦。
- **adjust 锁内跨 await**:先重构移出锁(锁外算计划,锁内只 reconcile),测试无滞留/死锁。
- **乐观创建的空 run**:planning 态零任务由 FE 既有 planning 空态承接;失败回退已有 fail-soft(degenerate plan)。
- **JSON 裸显**:`kind:"text"` 仅作心跳/进度,UI 显示为「拟稿中…」,不直出 JSON。
- **推理不可用**:provider 无 ThinkingDelta 时,阶段叙述 + 计划逐步浮现兜底,体验仍可见。
- **主题**:全程 CSS 变量,跨 5 套主题不破。

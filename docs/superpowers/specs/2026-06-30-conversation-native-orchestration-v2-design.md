# 会话原生多 Agent 编排 v2 · 设计

> 状态:已与用户确认三大方向 fork(见下)。本文件是已批准设计的记录;实施计划见
> `docs/superpowers/plans/2026-06-30-conversation-native-orchestration-v2.md`。
> **底层引擎符合预期,零改**(per-run 锁/调度器/task-kind/规划/worker);本次只做「呈现 + 接线 + 入口」。

## 背景与诉求(用户原话)

> 「你的智能编排底层能力是符合预期的……但是 UI 实在太丑,会话交互体验、侧边栏交互都没有和标准的『会话』对齐……请把多 agent 编排的底层能力、编排画布 UI、编排修改/调整能力**直接复用迁移给「会话」**,并基于会话优秀的交互/UI**原生增强**,使功能更成熟。会话内容区依旧干净合理。进入会话后,**增加一个查看 agent 画布的功能入口**;点击弹出**悬浮画布**,**点节点把该节点上下文投射到会话内容区**,**点 main 回到主 agent 会话内容区**,**默认始终显示 main 内容区**。」

参照面:标准会话页(`/conversation/:id` → `ChatConversation` → `ChatLayout` + `NomiChat` + `NomiSendBox`)。

## 历史教训(必须规避)

2026-06-27「会话原生编排 P1-P6」失败的真因不是方向,而是**把编排控制塞进会话**(会话内状态条 `OrchestrationStatusStrip` / 三态模型选择器 / 右栏常驻 DAG + lead 主管会话胶水)→ 过度复杂 + bug 重灾 → 全量回退为专用 Tab(`[[conversation-native-orchestration-redesign]]`)。
**本设计的反制:内容区默认是一条干净的普通会话;编排只以「① 右栏一个 tab + ② 悬浮画布 + ③ 内容区投射切换」三种受控形态出现,绝不把控制散落进主聊天。** 且**无 IR / 节点图复活**(`[[visual-workflows-rewrite]]` 已 archive,勿重建)。

## 已确认的三大 fork

1. **发起方式 = 两者都要**:主 agent 自主 fan-out(ultracode,经 `caps_orchestrator.nomi_run_create`)**和**用户在会话内显式发起,均可。
2. **画布形态 = 右栏常驻实时预览 + 点击展开悬浮**:右栏新增一个「编排」tab 显示小号实时画布预览;点「展开」浮出大号悬浮画布,可收起回小预览。
3. **独立「智能编排」Tab = 移除**:删 `/orchestrator` 路由 + 侧栏 `SiderOrchestratorEntry` + Titlebar 特例;编排完全活在会话里。

## 核心数据模型 — 「会话即 run 的 main」

**一个会话可成为一个 run 的宿主(lead/main)。** 关联通过两条已存在但休眠的链路点亮:

- `orch_runs.lead_conv_id`(迁移 018 已有列,生产恒 `None`)= 发起会话的 `conversations.id`。**写路径免费,无迁移。**
- 该会话 `extra.orchestrator_run_id = <run_id>`(沿用既有 marker 约定:**lead 只带 `orchestrator_run_id` 故仍可见**;worker 带 `orchestrator_task_id` 被侧栏双过滤隐藏 —— `sqlite_conversation.rs:804` + `useConversationListSync.ts:133`)。

前端**已**在 `pages/conversation/index.tsx:30-37` 订阅 `conversation.listChanged(action:"updated")` 并 `mutate()` 重取会话。因此:后端在建 run 时写 `extra.orchestrator_run_id` **并广播 listChanged**,前端即刻拿到带 run 链接的会话对象 → 点亮画布入口。**无需新增反查端点 / 索引。**

> worker 转录链路同样现成:`TRunTask.conversation_id`(`orchestratorTypes.ts:151`)即 worker 会话 id;点节点 `OpenTaskPayload.task.conversation_id` → `conversation.get` → `ReadOnlyConversationView`。

## 已批准设计

### A. 后端接线(小;无迁移、无引擎改动)

1. **`ConversationService::link_orchestrator_run(conversation_id, run_id)`**(新方法,`nomifun-conversation/src/service.rs`):`update_extra` 合并 `{ "orchestrator_run_id": run_id }`(merge 语义,`service.rs:1024`)**并**广播 `conversation.listChanged(conversation_id, "updated", source)`(复用 `broadcast_list_changed`,`service.rs:2650`)。空 `conversation_id` 直接 no-op。**这是唯一的链接写入点(DRY),两条发起路径都调它。**
2. **Path A — `caps_orchestrator.create`**(`caps_orchestrator.rs:113`):
   - 读 `ctx.conversation_id`(`deps.rs:125`,`String`,空串守卫);非空时 `build_adhoc_request` 传 `lead_conv_id: Some(parsed)`(取代恒 `None` 的 `:269`)。
   - `create_adhoc` 返回 `run` 后,调 `deps.conversation_service.link_orchestrator_run(&ctx.conversation_id, &run.id)`(`deps.conversation_service`,`deps.rs:31`)。
   - autonomy 默认仍 `supervised`(agent 自主发起、无 Tab 审批位);Remote 仍 deny(`ORCHESTRATOR_DENY_SURFACES`)。
3. **Path B — `create_adhoc_run` 路由**(`nomifun-orchestrator/src/routes.rs:244`):请求体 `CreateAdhocRunRequest.lead_conv_id`(`api-types/orchestrator.rs:481`,已存在)由前端填当前会话 id;返回 run 后调编排 crate 自持的 `ConversationService::link_orchestrator_run`。autonomy 默认仍 `interactive`(用户在场审批)。
4. **不变量守护**:`link_orchestrator_run` 只 merge extra + 广播,**不**碰 model/name/pinned、**不**杀 agent task(故用 `update_extra` 而非重量级 `update`)。per-run 锁、规划、调度全不动。

### B. 前端 — 会话原生编排呈现层

复用既有(几乎零重写):`DagCanvas{runId,onOpenTask}`(chrome-free / 自取数 / lazy)、`ReadOnlyConversationView{conversation,hideSendBox,agent_name}`(自挂 `orchestrator-transcript` PreviewProvider)、`useRunLive(runId)`、`useLeadThinking(runId)`、`OpenTaskPayload`、`RunDecisionFeed`、`layoutDag`、`TaskNode`、`OrchestratorComposer`、`RunIntentBox`(adjust)、以及从 `RunView` 抽出的 `RunControls`/`RunTitleEditor`/`ViewToggle`/`STATUS_META`。

1. **`OrchestrationProvider`(会话域 context)**:由发起会话挂载,holds
   `{ runId | null, detail, refetch, leadThinking, projectedTaskId, projectTask(payload), returnToMain(), canvasOpen, openCanvas(), collapseCanvas() }`。
   `runId` 源 = `conversation.extra.orchestrator_run_id`(随 `listChanged` 重取而更新)。避免 header/rail/overlay/内容区四处 prop 钻取。
2. **右栏「编排」tab(`OrchestrationRailTab`)** — 经 `ChatSlider` 的 `extraTabs` 注入(nomi 会话),与 文件/变更/指标 并列:
   - 有 run:小号实时 `DagCanvas` 预览(只读缩略,`pointer-events` 视情)+「展开 ⤢」按钮 + 状态 pill(`STATUS_META`)+ `leadThinking.active` 时「规划中」指示。
   - 无 run:空态 + **「发起多 agent 编排」**(Path B 入口)→ 内嵌 `OrchestratorComposer`(模型范围 + 自主度 pill)→ `createAdhoc({ goal, model_range, autonomy, lead_conv_id: conversation.id })`。
3. **悬浮画布 `OrchestrationCanvasOverlay`**:展开态 = 可拖拽浮层,内含玻璃头(`RunTitleEditor` 行内重命名 + 状态 pill + `RunControls` 取消/暂停/恢复/批准/重规划 + `ViewToggle` 画布⟷决策)+ body(`DagCanvas`(含 main 节点)/ `RunDecisionFeed`)+ 底部 `RunIntentBox`(adjust 意图)。可「收起」成小浮标(缩略预览),再点展开。`React.lazy` 载 DagCanvas。
4. **DagCanvas「main」节点增强**:新增可选 `onOpenMain?: () => void`;为真时在 DAG 根上方渲染一个合成「main」节点(代表 lead/main 会话),边连向各根任务;点击 → `returnToMain()`。保持向后兼容(无 `onOpenMain` 则不渲染,行为同今)。主题/布局沿用现有 CSS 变量 + `data-theme` MutationObserver。
5. **内容区投射(`ConversationContentSwitcher`,在 `NomiConversationPanel` 内)**:
   - 默认 `projectedTaskId === null` → 渲染 `NomiChat`(主 agent,完整可发消息)。**默认即 main。**
   - 点画布节点 → `projectTask(payload)` 置 `projectedTaskId`,解析 `task.conversation_id`(`conversation.get`)→ 内容区**覆盖渲染** `ReadOnlyConversationView`(只读 + `hideSendBox`),顶部细 banner:`查看:<task.title> · [重跑] [转向…] · ← 返回 main`。
   - **`NomiChat` 始终挂载**(投射时 `display:none` 隐藏)以保住滚动/状态;`ReadOnlyConversationView` 自带独立 PreviewProvider,与主会话 preview 隔离。
   - banner 的「重跑」`runs.rerunTask`、「转向…」`runs.steer`(per-node 调整能力就地可达);「← 返回 main」/ 点 canvas main 节点 → `returnToMain()` 置 `null`。
6. **会话头部入口(`headerExtra`)**:run 存在时显示一枚 pill「🕸 agent 画布 · <状态>」→ `openCanvas()`(右栏可能收起时的可发现入口)。无 run 不显示,头部保持干净(per `ChatLayout` 既有 `headerExtra` 槽 + 三能力控件)。

### C. 移除独立编排 Tab

删:Router `/orchestrator`(`Router.tsx:12,168`)、`SiderOrchestratorEntry` + `SiderNav/index.ts` 导出 + `Sider/index.tsx` 接线、`Titlebar` 对 `/orchestrator` 的会话化特例(`Titlebar/index.tsx:155,193,199,201`)、页面壳 `pages/orchestrator/index.tsx`、`RunView.tsx`(先抽出可复用件)、`NewRunComposer.tsx`、`NewRunIntentBox.tsx`、`RunHistory.tsx`、`RunListRail/RunListRow`、`RunWorkspaceRail`(会话已有自己的工作区右栏,v1 不接 worker 文件)、i18n `siderNav.orchestrator`。
**保留并迁用**:`DagCanvas`/`TaskNode`/`layoutDag`/`ReadOnlyConversationView`/`WorkerTranscriptPanel`(投射逻辑参照)/`RunDecisionFeed`/`AgentRoster`(可选)/`RolePrecipitationPanel`(可选)/`OrchestratorComposer`/`RunIntentBox`/`useRunLive`/`useLeadThinking`/`useOrchestratorData`/`memberLabel`/`orchestratorTypes`/`orchestratorEvents`/ipcBridge `orchestrator` 命名空间(全部 REST/WS 方法保留)。

## 范围与边界(v1)

- **纳入**:两条发起、右栏预览 tab、悬浮画布(展开/收起)、main 节点、内容区投射(默认 main)、run 级控制(取消/暂停/恢复/批准/重规划/重命名)+ adjust 意图 + per-node 重跑/转向、移除独立 Tab、对齐会话视觉(玻璃头/rd-24 composer/CSS 变量主题)。
- **暂不**(记录,非阻塞):reassign/updateTaskSpec 富 inspector(节点改派/微调表单);worker 文件右栏;Path B 结果回灌主会话(Path A 由 agent `nomi_run_result` 自然回灌;Path B 结果暂留画布);移动端发起(编排=桌面)。

## 风险与对策

- **重蹈「塞会话」覆辙** → 内容区默认纯净;编排仅三受控形态;无会话内状态条/三态选择器。
- **run↔会话 live 不同步** → `link_orchestrator_run` 写 extra **并**广播 `listChanged`;FE 既有订阅自动重取。
- **投射丢主会话状态** → `NomiChat` 始终挂载、仅隐藏。
- **嵌套 PreviewProvider 冲突** → `ReadOnlyConversationView` 用独立 `orchestrator-transcript` namespace(既有约定)。
- **per-run 锁 / 引擎不变量** → 后端零改引擎;`link_orchestrator_run` 只触 extra + 广播。
- **主题** → DagCanvas/overlay 全 CSS 变量 + `data-theme` 观察;无硬编码(护 5 套主题)。
- **lead 会话误隐** → 侧栏过滤键为 `orchestrator_task_id`,lead 只带 `orchestrator_run_id` 故可见(已验证,勿改过滤键)。

## 约束(贯穿;来自标准规则与记忆)

无 IR/compile/节点图;禁 cargo fmt;禁合并 main;提交前 `git pull --rebase`(注意迁移号——本次**无新迁移**);push 仅在用户要求;中文 only;UI 必须漂亮;品牌 NomiFun;公开路由禁 extract `Extension<CurrentUser>`;icon-park 具名禁别名;交互元素 `<div role=button>` 非裸 `<button>`;Arco 弹窗经 `useArcoMessage`;无 `any`/`@ts-ignore`;用 `npm run typecheck`;per-run 锁绝不跨 LLM await;编排 WS 事件手镜像两端同步。

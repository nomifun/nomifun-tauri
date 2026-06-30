# 会话原生多 Agent 编排 v2 · 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development(每任务新实现者 + 对抗评审)。
> 前端任务另遵 frontend-design(UI 必须漂亮是硬验收门)。设计见
> `docs/superpowers/specs/2026-06-30-conversation-native-orchestration-v2-design.md`。

**Goal:** 把成熟的多 agent 编排(引擎/画布/调整能力)原生融入标准「会话」:会话即 run 的 main agent;右栏「编排」tab 实时预览 + 悬浮画布;点节点把 worker 转录投射进内容区,点 main 回主会话(默认 main);移除独立编排 Tab。

**Architecture:** 后端零改引擎,只点亮两条休眠链路(`orch_runs.lead_conv_id` + 会话 `extra.orchestrator_run_id`)并经一个 `link_orchestrator_run`(写 extra + 广播 `conversation.listChanged`)统一接线两条发起路径。前端复用 `DagCanvas`/`ReadOnlyConversationView`/`useRunLive`/`useLeadThinking`/`OpenTaskPayload`/`RunDecisionFeed`/`RunControls` 等,以「会话域 `OrchestrationProvider` + 右栏 tab + 悬浮画布 + 内容区投射切换」四件套呈现,并删除独立 `/orchestrator` 表面。**无 IR / 无节点图。**

**Tech Stack:** Rust(nomifun-conversation / nomifun-gateway / nomifun-orchestrator)+ React + Arco + UnoCSS + @xyflow/react。

## Global Constraints
- **无 IR / compile / 节点图**(已 archive,禁复活);编排表达仍为 `orch_run_tasks.kind`。**底层引擎零改动**(per-run 锁/调度/规划/worker)。
- **per-run 锁绝不跨 LLM await**;本计划不触锁路径。
- 编排 WS 事件**手镜像**两端(events.rs + orchestratorEvents.ts + ipcBridge);本计划不新增编排 WS 事件(复用 `conversation.listChanged` + 既有 `orchestrator.*`)。
- **本次无新迁移**(`lead_conv_id` 列、`extra` marker 均已存在)。
- 主题色一律 CSS 变量(`--bg-base`/`--bg-2`/`--color-border-3`/`rgb(var(--primary-6))` 等),禁硬编码(护 5 套主题);DagCanvas/overlay 沿用 `data-theme` MutationObserver。
- 前端:`npm run typecheck` 0 新错(以此为准,非 `npx tsc`);`bun run build`(vite)绿;locale 改动后 `check:i18n` 绿、en/zh 对称、regen `i18n-keys.d.ts`;icon-park 具名导入**禁别名**;交互元素 `<div role="button">` 非裸 `<button>`;Arco 弹窗经 `useArcoMessage`;**无 `any`/`@ts-ignore`**。
- 后端:**禁 cargo fmt**;只跑触碰 crate 的 nextest(收尾全量一次);新公开路由禁 extract `Extension<CurrentUser>`(本计划不加公开路由)。
- 流程:**禁合并 main**;提交前 `git pull --rebase`;push 仅用户要求时。
- 侧栏隐藏过滤键为 `orchestrator_task_id`(lead 只带 `orchestrator_run_id` 故可见)——**勿改该过滤键**。
- 复用既有:`DagCanvas`/`ReadOnlyConversationView`/`useRunLive`/`useLeadThinking`/`OpenTaskPayload`/`RunDecisionFeed`/`OrchestratorComposer`/`RunIntentBox`/`layoutDag`/`TaskNode`/ipcBridge `orchestrator` 命名空间。

---

### Task B1: `ConversationService::link_orchestrator_run`(extra 写入 + 广播)

**Files:**
- Modify: `crates/backend/nomifun-conversation/src/service.rs`(新增 `pub async fn link_orchestrator_run`)
- Test: 同 crate 既有 service 测试位置(`service.rs` 的 `#[cfg(test)]` 或既有 tests 模块)

**Interfaces:**
- Consumes:既有 `self.update_extra(conversation_id, patch)`(`service.rs:1024`,merge 语义)、`self.broadcast_list_changed(conversation_id, action, source)`(`service.rs:2650`,`pub(crate)`)、`serde_json::json!`。
- Produces:
  ```rust
  /// 把一个编排 run 关联到发起会话:merge `extra.orchestrator_run_id`(只触 extra,不碰 model/name/pinned、不杀 agent task)
  /// 并广播 conversation.listChanged(updated) 让前端重取会话。空 conversation_id => no-op。
  pub async fn link_orchestrator_run(&self, conversation_id: &str, run_id: &str) -> Result<(), AppError>;
  ```

- [ ] 写失败测试:建一个会话 → `link_orchestrator_run(&conv_id, "run_abc")` → 重读会话断言 `extra["orchestrator_run_id"] == "run_abc"` 且其它 extra 键(如 workspace)保留(merge 非替换);用既有捕获 broadcaster mock 断言广播了一条 `conversation.listChanged`(payload `action=="updated"`,`conversation_id` 匹配)。再断言空串 `link_orchestrator_run("", "run_x")` 为 `Ok(())` 且无广播。
- [ ] 运行确认失败(方法不存在)。
- [ ] 实现:空 `conversation_id` 早返回 `Ok(())`;否则 `self.update_extra(conversation_id, json!({"orchestrator_run_id": run_id})).await?;` 然后 `self.broadcast_list_changed(conversation_id, "updated", Some("orchestrator"));`(对齐 `update` 的广播调用风格)。
- [ ] 运行 `nextest -p nomifun-conversation` 相关用例,确认通过。
- [ ] 提交 `feat(conversation): link_orchestrator_run 关联 run 到发起会话(extra+广播)`。

---

### Task B2: Path A — caps_orchestrator 关联发起会话(主 agent 自主)

**Files:**
- Modify: `crates/backend/nomifun-gateway/src/caps_orchestrator.rs`(`create` 读 `ctx.conversation_id` → `lead_conv_id` + 调 `link_orchestrator_run`)
- Test: 同 crate 既有 caps_orchestrator 测试位置(已存在 `:672,:690` 等断言 `lead_conv_id` 的测试)

**Interfaces:**
- Consumes:`ctx.conversation_id: String`(`deps.rs:125`)、`deps.conversation_service`(`deps.rs:31`,`ConversationService`)、B1 `link_orchestrator_run`、既有 `build_adhoc_request`(`caps_orchestrator.rs:249`)、`create_adhoc`(`:164`)。
- Produces:`create` 在 `create_adhoc` 返回 `run` 后,若 `!ctx.conversation_id.is_empty()`:① `build_adhoc_request` 传 `lead_conv_id: ctx.conversation_id.parse::<i64>().ok()`(取代恒 `None` 的 `:269`);② `deps.conversation_service.link_orchestrator_run(&ctx.conversation_id, &run.id).await`(失败仅 warn 不阻断 run)。空 `conversation_id`(MCP/无会话调用)行为同今(`lead_conv_id=None`,不写 extra)。

- [ ] 写失败测试:用既有 caps 测试夹具,以非空 `ctx.conversation_id`(如 `"909"`)调 `create` → 断言返回 run 的 `lead_conv_id == Some(909)`;并断言对应会话被 `link_orchestrator_run`(extra 含 `orchestrator_run_id`,经捕获 conversation_service / broadcaster mock)。再以空 `ctx.conversation_id` 调用 → run 创建成功且 `lead_conv_id == None`、未写 extra(行为回归)。
- [ ] 运行确认失败。
- [ ] 实现:把 `build_adhoc_request` 的 `lead_conv_id` 由参数注入(签名加 `lead_conv_id: Option<i64>`,或在 `create` 内构造后覆写);`create` 内解析 `ctx.conversation_id` 一次,传给 request 构造并在 run 返回后调 `link_orchestrator_run`(`if let Err(e) = ... { warn!(...) }`)。autonomy 默认仍 `supervised`,Remote deny 不变。
- [ ] 运行 `nextest -p nomifun-gateway` 相关用例,确认通过(含既有 `lead_conv_id` 断言已适配)。
- [ ] 提交 `feat(gateway): caps_orchestrator 把发起会话关联为 run lead(Path A)`。

---

### Task B3: Path B — create_adhoc_run 路由关联会话(用户显式)

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/routes.rs`(`create_adhoc_run`:取 `body.lead_conv_id` → 调 `link_orchestrator_run`)
- Modify(若需):`crates/backend/nomifun-orchestrator/src/...`(确认/补 orchestrator 路由 state 持有 `ConversationService` 句柄)
- Test: 同 crate 既有 routes/run_service 测试位置

**Interfaces:**
- Consumes:`CreateAdhocRunRequest.lead_conv_id: Option<i64>`(`api-types/orchestrator.rs:481`,已存在 `#[serde(default)]`)、B1 `link_orchestrator_run`、既有 `create_adhoc`(`run_service.rs:182` 已透传 `lead_conv_id`)、既有 `spawn_plan_and_start`。
- Produces:`create_adhoc_run` 在 `create_adhoc` 返回(planning 态)后,若 `body.lead_conv_id` 为 `Some(id)`:调 orchestrator 路由 state 的 `conversation_service.link_orchestrator_run(&id.to_string(), &run.id).await`(失败仅 warn)。autonomy 默认仍 `interactive`(`:254-256`)。
- **句柄确认**:orchestrator 的 `build_orchestrator_state` 已建独立 `ConversationService`(用于 worker)。本任务先核对该 state 是否暴露该 service 给路由处理器;若否,把它纳入路由可达的 AppState(只读引用,不改引擎)。

- [ ] 写失败测试:以含 `lead_conv_id: Some(<conv>)` 的 `CreateAdhocRunRequest` 调 `create_adhoc_run`(用可阻塞 mock planner 保持 planning 态)→ 断言返回 run 的 `lead_conv_id==Some(conv)` 且该会话被 link(extra/broadcast,经 mock)。再以 `lead_conv_id: None` 调用 → 行为同今(不 link)。
- [ ] 运行确认失败。
- [ ] 实现:定位/补齐路由可达的 `ConversationService`;在 `create_adhoc_run` 返回前(或 spawn 外)对 `Some(lead_conv_id)` 调 `link_orchestrator_run`。不改 `spawn_plan_and_start` 的乐观返回时序。
- [ ] 运行 `nextest -p nomifun-orchestrator` 相关用例,确认通过。
- [ ] 提交 `feat(orchestrator): create_adhoc 路由把会话关联为 lead(Path B)`。

---

### Task F1: 前端类型/桥 + `useConversationRun` 钩子

**Files:**
- Modify: `ui/src/common/config/storage.ts`(nomi extra 类型 `:425-460` 加 `orchestrator_run_id?: string`;如缺再加 `orchestrator_task_id?: string`)
- Modify: `ui/src/common/types/orchestrator/orchestratorTypes.ts`(`TCreateAdhocRun` 加 `lead_conv_id?: number`)
- Modify: `ui/src/common/adapter/ipcBridge.ts`(`orchestrator.runs.createAdhoc` 透传 `lead_conv_id`——若已是整体 body 透传则无需改,确认即可)
- Create: `ui/src/renderer/pages/conversation/orchestration/useConversationRun.ts`
- Test: `npm run typecheck` 0 新错(前端无 vitest)

**Interfaces:**
- Consumes:`TChatConversation`(`storage.ts`)、`conversation.extra.orchestrator_run_id`、既有 `useRunLive(runId)`、`useLeadThinking(runId)`。
- Produces:
  ```ts
  interface ConversationRunState {
    runId: string | null;            // = conversation.extra.orchestrator_run_id ?? null
    detail: TRunDetail | null;       // useRunLive(runId)
    refetch: () => Promise<void>;
    leadThinking: LeadThinkingState; // useLeadThinking(runId)
    loading: boolean;
  }
  function useConversationRun(conversation: TChatConversation | null | undefined): ConversationRunState;
  ```
- runId 仅从 `extra.orchestrator_run_id` 派生(会话随 `listChanged` 重取自动更新,不在此钩子内订阅 listChanged)。`runId` 为 null 时 `useRunLive(undefined)` 静默。

- [ ] 加类型字段(extra + TCreateAdhocRun);确认 ipcBridge createAdhoc 透传 lead_conv_id(必要时改)。
- [ ] 实现 `useConversationRun`(读 extra → runId → useRunLive + useLeadThinking)。
- [ ] `npm run typecheck` = 0 新错。
- [ ] 提交 `feat(conversation/ui): 会话↔run 类型接线 + useConversationRun`。

---

### Task F2: 抽出 RunView 可复用件(RunControls/RunTitleEditor/ViewToggle/STATUS_META)

**Files:**
- Create: `ui/src/renderer/pages/orchestrator/RunDetail/RunControls.tsx`(从 `RunView.tsx:300-402` 抽出)
- Create: `ui/src/renderer/pages/orchestrator/RunDetail/RunTitleEditor.tsx`(从 `:159-243` 抽出)
- Create: `ui/src/renderer/pages/orchestrator/RunDetail/ViewToggle.tsx`(从 `:102-150` 抽出)
- Create: `ui/src/renderer/pages/orchestrator/RunDetail/runStatusMeta.ts`(从 `:42-50` 抽出 `STATUS_META`)
- Modify: `ui/src/renderer/pages/orchestrator/RunDetail/RunView.tsx`(改为 import 抽出件;保持现行为不变,直到 F9 删除)
- Test: `npm run typecheck` 0 新错

**Interfaces:**
- Produces(纯位置迁移,签名不变):
  ```ts
  // RunControls.tsx
  interface RunControlsProps { detail: TRunDetail; refetch: () => Promise<void>; onReplan: () => void; }
  // RunTitleEditor.tsx
  interface RunTitleEditorProps { runId: string; goal: string; onRename: (goal: string) => Promise<void>; }
  // ViewToggle.tsx
  type RunViewMode = 'conversation' | 'canvas';
  interface ViewToggleProps { value: RunViewMode; onChange: (m: RunViewMode) => void; }
  // runStatusMeta.ts
  export const STATUS_META: Record<string, { label: string; color: string }>; // 既有形状
  ```
- **纯重构**:行为/视觉零变化;`RunView` 仍编译可用(F9 才删 RunView 本身)。

- [ ] 抽出四件到独立文件(保持 i18n key / 类名 / CSS 变量原样);`RunView` 改 import。
- [ ] `npm run typecheck` = 0 新错。
- [ ] 提交 `refactor(orchestrator/ui): 抽出 RunControls/RunTitleEditor/ViewToggle/STATUS_META 供会话复用`。

---

### Task F3: 会话域 `OrchestrationProvider` + 接入 NomiConversationPanel

**Files:**
- Create: `ui/src/renderer/pages/conversation/orchestration/OrchestrationContext.tsx`(provider + `useOrchestration()` hook)
- Modify: `ui/src/renderer/pages/conversation/components/ChatConversation.tsx`(`NomiConversationPanel` `:140-201` 外包 `OrchestrationProvider`,传入 `conversation`)
- Test: `npm run typecheck` 0 新错

**Interfaces:**
- Consumes:F1 `useConversationRun(conversation)`、`OpenTaskPayload`(`orchestrator/RunDetail/DagCanvas` 导出)。
- Produces:
  ```ts
  interface OrchestrationContextValue {
    conversationId: number;
    runId: string | null;
    detail: TRunDetail | null;
    refetch: () => Promise<void>;
    leadThinking: LeadThinkingState;
    // 内容区投射
    projectedTaskId: string | null;
    projectTask: (payload: OpenTaskPayload) => void;
    returnToMain: () => void;
    // 悬浮画布
    canvasOpen: boolean;       // 展开态
    openCanvas: () => void;
    collapseCanvas: () => void;
  }
  function useOrchestration(): OrchestrationContextValue; // provider 外调用抛错
  function useOrchestrationSafe(): OrchestrationContextValue | null;
  ```
- `projectTask` 存 `payload.task.id` + 缓存 payload(供内容区解析 conversation_id);换 run / `returnToMain` 重置 `projectedTaskId=null`。**默认 `projectedTaskId=null`(即 main)、`canvasOpen=false`。**

- [ ] 实现 context（state：projectedTaskId / canvasOpen；接 useConversationRun）。
- [ ] `NomiConversationPanel` 外层包 `OrchestrationProvider`（仅 nomi 会话）。
- [ ] `npm run typecheck` = 0 新错。
- [ ] 提交 `feat(conversation/ui): 会话域 OrchestrationProvider(run/投射/画布状态)`。

---

### Task F4: DagCanvas「main」节点增强

**Files:**
- Modify: `ui/src/renderer/pages/orchestrator/RunDetail/DagCanvas.tsx`(可选 `onOpenMain` + 合成 main 节点)
- Create(若需独立节点样式): `ui/src/renderer/pages/orchestrator/RunDetail/nodes/MainNode.tsx`
- Test: `npm run typecheck` 0 新错 + 手测

**Interfaces:**
- Consumes:既有 react-flow 节点构建(`:373-463`)、`layoutDag`、主题 state(`:323-342`)。
- Produces:`DagCanvasProps` 加可选 `onOpenMain?: () => void`、`mainActive?: boolean`(投射时 main 高亮态可视)。为真时:在拓扑根任务上方注入一个 `type:'main'` 合成节点(label「main · 主 agent」),从 main → 每个根任务连边;点击 `data.onOpen=() => onOpenMain()`。`onOpenMain` 缺省则不渲染 main 节点(**向后兼容,行为同今**)。

- [ ] 实现 main 节点(注册 `NODE_TYPES.main`;布局把 y 偏移腾出顶部一行;边 main→roots;主题色走现有 flowColors/CSS 变量)。
- [ ] `npm run typecheck` = 0 新错;手测:有 onOpenMain 时画布顶部出现 main 节点且点击触发回调,无 onOpenMain 时画布同旧。
- [ ] 提交 `feat(orchestrator/ui): DagCanvas 可选 main 节点(回主 agent)`。

---

### Task F5: 右栏「编排」tab(实时预览 + 展开 + 空态发起 Path B)

**Files:**
- Create: `ui/src/renderer/pages/conversation/orchestration/OrchestrationRailTab.tsx`
- Modify: `ui/src/renderer/pages/conversation/components/ChatSlider.tsx`(nomi 分支 `extraTabs` 追加 `{ key:'orchestration', title:t('...'), content:<OrchestrationRailTab/> }`,`:56-62`)
- Test: `npm run typecheck` 0 新错 + 手测

**Interfaces:**
- Consumes:`useOrchestration()`(F3)、`DagCanvas`(F4,小号只读预览 + `onOpenMain`/`onOpenTask` 走 context)、`OrchestratorComposer`(空态发起)、`ipcBridge.orchestrator.runs.createAdhoc`、`STATUS_META`(F2)、`useModelRange`。
- Produces:`OrchestrationRailTab`(无 props,纯读 context):
  - **有 run**:状态 pill + `leadThinking.active` 时「规划中…」+ 小号 `DagCanvas`(`React.lazy`,容器高度受限,节点点击/ main → context.projectTask/returnToMain;另显「展开 ⤢」按钮 → `openCanvas()`)。
  - **无 run**:空态文案 + `OrchestratorComposer`(模型范围 + 自主度 pill,800px 不强制——rail 窄,用紧凑布局)→ 提交调 `createAdhoc({ goal, model_range:buildModelRange(...), autonomy, lead_conv_id: conversationId })`(后端写 extra+广播 → 会话重取 → runId 点亮)。
- 视觉:rd 卡 + CSS 变量主题;与 文件/变更/指标 tab 同观感。

- [ ] 实现 OrchestrationRailTab(两态);ChatSlider 注入 extraTab(仅 nomi)。
- [ ] icon-park 具名禁别名;发送/按钮用 `<div role=button>` 或 Arco Button;Arco 弹窗经 useArcoMessage;无 any。
- [ ] `npm run typecheck` = 0 新错;手测:无 run 见发起卡、有 run 见实时预览 + 展开钮。
- [ ] 提交 `feat(conversation/ui): 右栏「编排」tab 实时预览 + 空态发起`。

---

### Task F6: 悬浮画布 `OrchestrationCanvasOverlay`(展开/收起 + 控制 + adjust)

**Files:**
- Create: `ui/src/renderer/pages/conversation/orchestration/OrchestrationCanvasOverlay.tsx` + `.module.css`
- Modify: `ui/src/renderer/pages/conversation/components/ChatConversation.tsx`(`NomiConversationPanel` 内挂 overlay,受 `canvasOpen` 控制)
- Test: `npm run typecheck` 0 新错 + 手测

**Interfaces:**
- Consumes:`useOrchestration()`、`DagCanvas`(F4 含 main 节点)、`RunControls`/`RunTitleEditor`/`ViewToggle`/`STATUS_META`(F2)、`RunDecisionFeed`、`RunIntentBox`(adjust)、`useLeadThinking`。
- Produces:`OrchestrationCanvasOverlay`(读 context):
  - **展开态**(`canvasOpen`):浮层(绝对定位覆盖内容区,可拖拽移动 + resize;`z-index` 高于内容区低于全局 Modal)。玻璃头(`chat-layout-header--glass` 观感):`RunTitleEditor`(rename→`runs.rename`+refetch)+ 状态 pill + `RunControls`(取消/暂停/恢复/批准/重规划)+ `ViewToggle`(画布⟷决策)+「收起 —」按钮(→`collapseCanvas()`)。body:`viewMode==='canvas'`→`DagCanvas{runId, onOpenTask:projectTask, onOpenMain:returnToMain, mainActive:projectedTaskId===null}`;`'conversation'`→`RunDecisionFeed{detail, onSelectTask:projectTask, ...}`。底部 `RunIntentBox`(adjust 意图→`runs.adjustRun`)。
  - **收起态**:小浮标(缩略状态 chip:目标截断 + 状态点 + `leadThinking.active` 脉冲),点击 → `openCanvas()`。
  - run 不存在则 overlay 不渲染。
- 视觉:全 CSS 变量;rd-16/24;阴影柔和;主题跟随。

- [ ] 实现 overlay(展开/收起两态 + 拖拽 + 头部控制 + body 双视图 + adjust 底栏);ChatConversation 挂载。
- [ ] DagCanvas `React.lazy`;无 any;Arco 弹窗 useArcoMessage;按钮 `<div role=button>`/Arco Button。
- [ ] `npm run typecheck` = 0 新错;手测:展开/收起、控制按钮按状态门控、adjust 提交、画布⟷决策切换。
- [ ] 提交 `feat(conversation/ui): 悬浮 agent 画布(控制 + adjust + 收起小浮标)`。

---

### Task F7: 内容区投射切换(默认 main + worker 只读 + 返回 banner)

**Files:**
- Create: `ui/src/renderer/pages/conversation/orchestration/ConversationContentSwitcher.tsx`
- Create: `ui/src/renderer/pages/conversation/orchestration/ProjectedWorkerView.tsx`(解析 conversation_id + ReadOnlyConversationView + banner)
- Modify: `ui/src/renderer/pages/conversation/components/ChatConversation.tsx`(`NomiConversationPanel` 的内容 children 包一层 switcher)
- Test: `npm run typecheck` 0 新错 + 手测

**Interfaces:**
- Consumes:`useOrchestration()`(`projectedTaskId`/缓存 payload/`returnToMain`)、`ipcBridge.conversation.get`、`ReadOnlyConversationView`(`conversation`,`hideSendBox`,`agent_name`)、`ipcBridge.orchestrator.runs.{rerunTask,steer}`。
- Produces:
  - `ConversationContentSwitcher`:**始终挂载 `NomiChat`(props.children)**,投射时 `display:none` 隐藏(保状态);`projectedTaskId!=null` 时**覆盖渲染** `ProjectedWorkerView`。
  - `ProjectedWorkerView{ payload: OpenTaskPayload }`:用 `payload.task.conversation_id` → `conversation.get.invoke({id})` → `ReadOnlyConversationView`;`conversation_id==null` → 「该 agent 尚未开始」空态。顶部 banner(rd + CSS 变量):`查看:<task.title>` · `[重跑]`(`runs.rerunTask`)· `[转向…]`(弹小输入→`runs.steer`)· `[← 返回 main]`(`returnToMain`)。
- **默认 main**:`projectedTaskId===null` → 只见 `NomiChat`。

- [ ] 实现 switcher + ProjectedWorkerView(NomiChat 常挂、worker 只读覆盖、banner 三动作)。
- [ ] NomiConversationPanel 内容包 switcher;`NomiChat` 仍在同一 PreviewProvider/Conversation 作用域内。
- [ ] `npm run typecheck` = 0 新错;手测:点节点 → 内容区变 worker 只读 + banner;返回 main → 复原且主会话状态保留。
- [ ] 提交 `feat(conversation/ui): 内容区投射(默认 main / worker 只读 / 返回)`。

---

### Task F8: 会话头部「agent 画布」入口 pill

**Files:**
- Modify: `ui/src/renderer/pages/conversation/components/ChatConversation.tsx`(`NomiConversationPanel` 组 `headerExtra`:run 存在时插入入口 pill)
- Create(可选): `ui/src/renderer/pages/conversation/orchestration/CanvasEntryPill.tsx`
- Test: `npm run typecheck` 0 新错 + 手测

**Interfaces:**
- Consumes:`useOrchestration()`(`runId`/`detail.run.status`/`openCanvas`/`leadThinking.active`)、`STATUS_META`、既有 `ChatLayout` `headerExtra` 槽。
- Produces:`CanvasEntryPill`:`runId` 存在时渲染一枚 pill「🕸 agent 画布 · <状态>」(`leadThinking.active` 脉冲),点击 `openCanvas()`;无 run 不渲染(头部保持干净)。与既有 `CronJobManager`/能力控件并存于 `headerExtra`。

- [ ] 实现 pill;接入 NomiConversationPanel 的 `headerExtra`(与现有 extra 内容合并,不覆盖)。
- [ ] icon-park 具名禁别名;`<div role=button>`/Arco Button;移动端 headerExtra 走既有 portal 逻辑不破。
- [ ] `npm run typecheck` = 0 新错;手测:有 run 见 pill 点开画布、无 run 不见。
- [ ] 提交 `feat(conversation/ui): 会话头部 agent 画布入口 pill`。

---

### Task F9: 移除独立「智能编排」表面

**Files:**
- Delete: `ui/src/renderer/pages/orchestrator/index.tsx`、`RunDetail/RunView.tsx`、`NewRunComposer.tsx`、`NewRunIntentBox.tsx`、`RunHistory.tsx`、`RunDetail/RunWorkspaceRail.tsx`、以及 RunListRail/RunListRow(在 index.tsx 内则随之删)
- Delete: `ui/src/renderer/components/layout/Sider/SiderNav/SiderOrchestratorEntry.tsx`
- Modify: `ui/src/renderer/components/layout/Router.tsx`(删 `/orchestrator` import + Route `:12,:168`)
- Modify: `ui/src/renderer/components/layout/Sider/SiderNav/index.ts`(删导出 `:10`)
- Modify: `ui/src/renderer/components/layout/Sider/index.tsx`(删 import/handler/渲染 `:24,:87,:176-182`)
- Modify: `ui/src/renderer/components/layout/Titlebar/index.tsx`(删 `/orchestrator` 会话化特例 `:155,:193,:199,:201` 等)
- Test: `npm run typecheck` 0 新错 + `vite build` 绿

**Interfaces:**
- **保留**:`RunDetail/DagCanvas.tsx`、`TaskNode`、`layoutDag.ts`、`ReadOnlyConversationView.tsx`、`WorkerTranscriptPanel.tsx`(若仍被引则保留,否则删)、`RunDecisionFeed.tsx`、`AgentRoster.tsx`、`RolePrecipitationPanel.tsx`、F2 抽出的四件、`OrchestratorComposer.tsx`、`RunIntentBox.tsx`、`useRunLive.ts`、`useLeadThinking.ts`、`useOrchestratorData.ts`、`memberLabel.ts`、`orchestratorTypes.ts`、`orchestratorEvents.ts`、ipcBridge `orchestrator` 命名空间。
- 删除后全局无悬挂 import;`/orchestrator` 路由不可达;侧栏/Titlebar 无编排特例。

- [ ] 删页面壳 + 导航/路由/Titlebar 接线;逐一清理悬挂 import(typecheck 驱动)。
- [ ] 核对保留组件均仍被会话侧引用或为公共件(无误删 DagCanvas 等)。
- [ ] `npm run typecheck` = 0 新错;`bun run build`(vite)绿。
- [ ] 提交 `refactor: 移除独立智能编排 Tab(路由/侧栏/Titlebar/页面壳),编排归并会话`。

---

### Task F10: i18n 文案(中英对称)+ 类型 + build 收尾

**Files:**
- Modify: `ui/src/renderer/services/i18n/locales/zh-CN/*.json` + `en-US/*.json`(新增会话编排文案:rail tab 标题/空态发起/状态/画布展开收起/投射 banner/重跑/转向/返回 main/头部入口;删 `siderNav.orchestrator` 等已移除键)
- Modify(生成): `ui/src/renderer/services/i18n/i18n-keys.d.ts`(regen)
- Test: `check:i18n` 绿 + `npm run typecheck` 0 + `vite build` 绿

**Interfaces:**
- Consumes:F5/F6/F7/F8 引用的所有新 key。
- Produces:en/zh **对称**新增(命名贴合既有 `orchestrator.*` 结构,实施时核对实际引用 key);删除 F9 移除的孤儿 key。

- [ ] 汇总新 key,en/zh 对称补齐;删孤儿键;无缺失。
- [ ] regen `i18n-keys.d.ts`(根 `gen:i18n` 脚本);`check:i18n` 绿(根脚本)。
- [ ] `npm run typecheck` = 0;`bun run build` 绿。
- [ ] 提交 `feat(conversation/ui): 会话编排 i18n 文案(中英对称)+ 清理`。

---

## Self-Review / 风险
**覆盖:** 发起(两路)→ B2/B3 + F5(空态)/F8;画布入口 → F5(rail)/F8(header);悬浮画布 → F6 + F4(main 节点);投射 + 默认 main → F7;调整能力 → F6(run 级 + adjust)/F7(per-node 重跑/转向);移除 Tab → F9;视觉对齐 → F5/F6/F7/F8 全 CSS 变量 + 玻璃头 + 复用会话观感;接线 → B1 + F1/F3。
**不变量:** 引擎零改;per-run 锁不触;无 IR/节点图;`link_orchestrator_run` 只 extra+广播(不杀 task);侧栏过滤键不动;`ReadOnlyConversationView` 独立 PreviewProvider;DagCanvas main 节点向后兼容;NomiChat 投射时仅隐藏不卸载;无新迁移/无新编排 WS 事件。
**风险:** ① 重蹈塞会话——三受控形态 + 内容区默认纯净;② live 不同步——extra+广播复用既有订阅;③ B3 路由缺 ConversationService 句柄——任务内先核对/补;④ overlay 拖拽 z-index 与 Modal/Preview 冲突——限定层级 + 限内容区范围;⑤ 嵌套 PreviewProvider——独立 namespace;⑥ 主题硬编码——全 CSS 变量审查。

## Execution Handoff
SDD:B1→B2→B3→F1→F2→F3→F4→F5→F6→F7→F8→F9→F10,每任务新实现者 + 对抗评审,账本 `.superpowers/sdd/progress.md`。
后端 sonnet(B1 机械、B2/B3 标准);前端 frontend-design(标准;F6/F7 keystone 视觉用 opus 评审)。最后 whole-branch opus 终评审。
禁 IR / cargo fmt / 合并 main。

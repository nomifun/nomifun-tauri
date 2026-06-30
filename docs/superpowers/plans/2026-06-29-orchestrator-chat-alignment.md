# 智能编排页对齐会话页 UI/交互 + 主 Agent 编排思考流 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development(每任务新实现者 + 对抗评审)。
> 前端任务另需 frontend-design(UI 必须漂亮是硬验收门)。设计见
> `docs/superpowers/specs/2026-06-29-orchestrator-chat-alignment-design.md`。

**Goal:** 把智能编排页的 UI/交互全面对齐会话页(rd-24px chat 风 composer + 玻璃头 + 对齐的列表/决策流),并实时呈现
主 agent 的编排思考过程(推理优先 + 阶段叙述),用乐观创建消除提交后的空挡。

**Architecture:** 在既有多 agent 编排引擎上增强。后端:`one_shot_completion` 底层本就流式(`streaming_completion`
的 `on_delta`),把被丢弃的 `TextDelta`/`ThinkingDelta` 经新 WS 事件 `orchestrator.run.leadThinking` fan out;
`create_adhoc` 路由改为立即返回 + 后台 spawn 规划;adjust 把 lead LLM 调用移出 per-run 锁后再流式。前端:复用会话页
视觉语言(`useInputFocusRing`、`.send-button-custom`、`.sendbox-model-btn`、`chat-layout-header--glass`、800px 列)
重塑 composer/玻璃头/列表/决策流,并新增 `useLeadThinking` 独立订阅渲染流式思考气泡。**无 IR / 无节点图。**

**Tech Stack:** Rust(nomifun-orchestrator / nomifun-ai-agent / nomifun-realtime)+ React + Arco Design + UnoCSS。

## Global Constraints
- **无 IR / compile / typed-graph / 节点图**(已撤回方向,禁复活);编排表达仍为 `orch_run_tasks.kind`。
- **per-run 锁(`RunLocks`,tokio Mutex)绝不可跨 LLM await**;锁内只做纯 DB 变更。
- 编排 WS 事件**手镜像**:新事件须同时加 `crates/.../nomifun-orchestrator/src/events.rs`、
  `ui/src/common/types/orchestrator/orchestratorEvents.ts`、`ui/src/common/adapter/ipcBridge.ts` 的 `runEvents`。
- 主题色**一律走 CSS 变量**(`--bg-base`/`--bg-2`/`--color-border-3`/`--color-border-2`/`rgb(var(--primary-6))` 等),
  禁硬编码(护现役 5 套主题)。
- 前端:`npm run typecheck` 必须 0 新错(`npx tsc` 误报 0,以 `npm run typecheck` 为准);`bun run build`(或既有 build 脚本)绿;
  locale 改动后 `check:i18n` 绿且 en-US/zh-CN 对称、regen `i18n-keys.d.ts`;icon-park 具名导入**禁起别名**;
  交互元素用 `<div role="button">` 非裸 `<button>`;Arco 弹窗经 `useArcoMessage`;**无 `any` / 无 `@ts-ignore`**。
- 后端:**禁 cargo fmt**;只跑触碰 crate 的 nextest(收尾全量一次);新公开路由处理器**禁** extract `Extension<CurrentUser>`(如新增公开路由)。
- 流程:**禁合并 main**;提交前 `git pull --rebase`(注意迁移号撞号);push 仅在用户要求时。
- 复用既有:`useInputFocusRing`(零依赖)、`.send-button-custom`/`.sendbox-model-btn`(sendbox.css)、
  `EventBroadcaster` 总线、`AgentStreamEvent::Thinking`→`MessageThinking` 渲染原语。

---

### Task B1: 后端 leadThinking 事件

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/events.rs`(`OrchestratorRunEventEmitter` 加 `emit_lead_thinking`)
- Test: 同 crate 既有 events/emitter 测试位置(沿用现有测试文件)

**Interfaces:**
- Consumes:既有 `OrchestratorRunEventEmitter`(持 `Arc<dyn EventBroadcaster>`)、`WebSocketMessage::new(name, payload)`。
- Produces:新 WS 事件名 **`orchestrator.run.leadThinking`**,payload 形状:
  ```jsonc
  { "run_id": "<id>", "phase": "plan|adjust|summarize", "kind": "reasoning|text|phase",
    "delta": "<增量,可选>", "content": "<整段,可选(如阶段叙述key或done的最终内容)>", "done": false }
  ```
  新方法签名(与既有 emit_* 同风格):
  `pub fn emit_lead_thinking(&self, run_id: &str, phase: &str, kind: &str, delta: Option<&str>, content: Option<&str>, done: bool)`

- [ ] 写失败测试:emit_lead_thinking 经 broadcaster 广播一条 name=`orchestrator.run.leadThinking` 的消息,payload 含 run_id/phase/kind/done,delta/content 按 Option 出现/省略(用既有 mock/捕获 broadcaster 断言,参照现有 emit_* 测试写法)。
- [ ] 运行测试确认失败(方法不存在)。
- [ ] 实现 `emit_lead_thinking`(序列化 payload,省略 None 字段,broadcast)。沿用现有 emit_* 的序列化/广播套路。
- [ ] 运行该 crate nextest 相关用例,确认通过。
- [ ] 提交 `feat(orchestrator): leadThinking WS 事件(主agent 规划思考流承载)`。

---

### Task B2: 流式转发 lead 增量(text + reasoning,带合并)

**Files:**
- Modify: `crates/backend/nomifun-ai-agent/src/factory/provider_config.rs`(增加可转发 thinking 的流式入口)
- Modify: `crates/backend/nomifun-orchestrator/src/plan.rs`(`PlanProducer` 接入 lead-thinking sink;LlmPlanProducer 用流式入口替换 one_shot 空回调)
- Modify: `crates/backend/nomifun-orchestrator/src/run_service.rs`(把 sink 从引擎传入 produce/adjust/summarize 调用点)
- Modify: `crates/backend/nomifun-orchestrator/src/engine.rs`(`RunEngineDeps` 已持 emitter;构造 sink 闭包 `|kind, delta| emitter.emit_lead_thinking(run_id, phase, kind, Some(delta), None, false)`,带合并节流)
- Test: plan.rs / provider_config.rs 既有测试位置

**Interfaces:**
- Consumes:B1 的 `emit_lead_thinking`;既有 `streaming_completion(cfg, system, msgs, max, on_delta: impl FnMut(&str))`、
  `LlmEvent::{TextDelta, ThinkingDelta, Done, Error}`、`drain_text_response_with`。
- Produces:
  - provider_config:新增 `streaming_completion_kinded(cfg, system, msgs, max, on_delta: impl FnMut(DeltaKind, &str))`
    (或等价:回调带 `DeltaKind { Text, Reasoning }`),内部 drain 同时把 `TextDelta`→Text、`ThinkingDelta`→Reasoning 转发;
    `one_shot_completion` 保持不变(仍 `|_| {}` 包装),不破坏既有调用方。
  - plan.rs:`PlanProducer` 的 produce/adjust/summarize 增加一个可选 sink 参数(`Option<&mut dyn FnMut(LeadDeltaKind, &str)>`
    或 `Option<Arc<dyn Fn(&str,&str)+Send+Sync>>`,选与现有 trait 兼容的形态;**默认 None 时行为与今完全一致**),
    LlmPlanProducer 在有 sink 时调 `streaming_completion_kinded`,无 sink 时维持 one_shot。
- 合并节流:引擎侧 sink 闭包按「每 ≥80ms 或累计 ≥48 字符 flush 一次」聚合后再 `emit_lead_thinking`,末尾 flush 残余;
  常量集中定义、注释说明防 WS 洪泛。

- [ ] 写失败测试①(provider_config):用既有 mock provider 发 TextDelta+ThinkingDelta+Done,断言 `streaming_completion_kinded` 的回调按 kind 分别收到、返回的最终拼接文本 == 仅 TextDelta 拼接(与 one_shot 等价)。
- [ ] 写失败测试②(plan.rs):LlmPlanProducer.produce 传入捕获 sink(用 mock provider 注入 deltas),断言 sink 收到增量且解析出的 PlannedDag 与无 sink 路径一致(fail-soft 不变)。
- [ ] 运行确认失败。
- [ ] 实现 `streaming_completion_kinded` + `DeltaKind`;`drain_*` 转发 thinking。
- [ ] 实现 plan.rs sink 形参 + LlmPlanProducer 分支(有 sink 走 kinded,无 sink 走 one_shot);run_service 调用点透传 sink;engine 构造合并节流 sink 闭包(phase 由调用上下文给定)。
- [ ] 运行 nomifun-ai-agent + nomifun-orchestrator 相关 nextest,确认通过。
- [ ] 提交 `feat(orchestrator): 流式转发 lead 规划增量(text+reasoning,合并防洪泛)`。

---

### Task B3: 乐观创建立即返回 + 阶段叙述

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/routes.rs`(`create_adhoc_run`:create_adhoc 后**立即返回** run;`plan()`+`engine.start` 改为后台 `tokio::spawn`)
- Modify: `crates/backend/nomifun-orchestrator/src/run_service.rs` 和/或 `engine.rs`(在 `plan()` 关键节点发 `kind:"phase"` 叙述事件:planning-started / decomposing / assigning / plan-ready;plan-ready 复用既有 `emit_run_plan_updated`)
- Test: routes/run_service 既有测试位置;`caps_orchestrator.rs`(MCP 前门)同步保持一致或显式不变

**Interfaces:**
- Consumes:B1 `emit_lead_thinking`(phase kind)、既有 `create_adhoc`/`plan`/`engine.start`、`emit_run_plan_updated`。
- Produces:`create_adhoc_run` 返回时机 = run 已持久化(planning 态)即返回;规划在后台任务跑。
  阶段叙述事件序列(phase="plan"):`kind:"phase"` content ∈ {`"planning_started"`,`"decomposing"`,`"assigning"`}(语义 key,
  **文案在前端 i18n**,后端只发 key);plan-ready 仍由 `emit_run_plan_updated` 表达。

- [ ] 写失败测试:create_adhoc_run 路由返回的 run 状态为 planning 且**在 plan 完成前**返回(用可阻塞/计时的 mock planner 或断言返回时任务数为 0);后台规划完成后发出 planUpdated。
- [ ] 写失败测试:plan() 过程中按序发出 phase 叙述事件(捕获 emitter 断言含 planning_started/decomposing/assigning)。
- [ ] 运行确认失败。
- [ ] 实现:routes 立即返回 + spawn(spawn 内 plan 失败走既有 fail-soft,不 panic;错误经 run 状态/事件反映);plan()/assign 节点插入 phase 叙述 emit。
- [ ] 确认 `caps_orchestrator.rs` 第二入口语义一致(同样后台规划或显式注明差异)。
- [ ] 运行相关 nextest,确认通过。
- [ ] 提交 `feat(orchestrator): 乐观创建立即返回 + 规划阶段叙述事件(消空挡)`。

---

### Task B4: adjust 路径锁外流式(守不变量)

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/engine.rs`(`RunEngine::adjust`:把 `planner.adjust` 的 LLM await 移出 per-run 锁)
- Modify: `crates/backend/nomifun-orchestrator/src/run_service.rs`(拆分 adjust:`compute_adjusted_plan`(锁外,含 LLM)+ `apply_adjusted_plan`(锁内 reconcile_run_plan + 重激活))
- Test: 既有 adjust/reconcile 测试位置

**Interfaces:**
- Consumes:B2 sink(adjust phase 流式)、既有 `planner.adjust`、`reconcile_run_plan`、`RunLocks.for_run`。
- Produces:`RunEngine::adjust` 新结构:① 锁外 `run_service.compute_adjusted_plan(user, run_id, intent, sink)`(快照现态 + lead LLM + 解析,流式发 leadThinking phase="adjust");② `let _guard = lock.lock().await;` 锁内 `run_service.apply_adjusted_plan(...)`(reconcile + 重激活)+ `engine.start(!is_running)`。**锁内零 LLM await。**

- [ ] 写失败测试:断言 adjust 期间 per-run 锁未跨 LLM await(用注入延迟的 mock planner + 并发 rerun/loop 终止判定,验证不被阻塞 / 无死锁);adjust 语义(保留/新增/移除 + deps 重建 + 完成产出保留)与重构前一致。
- [ ] 运行确认失败。
- [ ] 实现拆分:compute(锁外)/apply(锁内);adjust phase 流式接 B2 sink。
- [ ] 运行 nomifun-orchestrator 相关 nextest(含既有 adjust/reconcile/锁用例),确认通过。
- [ ] 提交 `refactor(orchestrator): adjust 的 lead LLM 调用移出 per-run 锁 + adjust 阶段流式`。

---

### Task F1: 前端线契约 + useLeadThinking 钩子

**Files:**
- Modify: `ui/src/common/types/orchestrator/orchestratorEvents.ts`(镜像 `orchestrator.run.leadThinking` 事件类型)
- Modify: `ui/src/common/adapter/ipcBridge.ts`(`orchestrator.runEvents.leadThinking = wsEmitter('orchestrator.run.leadThinking')`)
- Create: `ui/src/renderer/pages/orchestrator/useLeadThinking.ts`
- Test: 无可跑 vitest(本项目前端无单测);以 `npm run typecheck` 0 为门

**Interfaces:**
- Consumes:B1 事件 payload 形状;既有 `wsEmitter`、`useRunLive` 模式。
- Produces:
  ```ts
  type LeadThinkingPhase = 'plan' | 'adjust' | 'summarize';
  type LeadThinkingKind = 'reasoning' | 'text' | 'phase';
  interface LeadThinkingState {
    phase: LeadThinkingPhase | null;
    reasoning: string;      // 累积 reasoning 文本
    phaseKeys: string[];    // 已收到的阶段叙述 key(planning_started/decomposing/assigning…)
    active: boolean;        // 流进行中
    textHeartbeat: boolean; // 收到过 text 增量(用于"拟稿中…",不存 JSON 内容)
  }
  function useLeadThinking(runId: string | null): LeadThinkingState;
  ```
- **独立订阅**:只订 `runEvents.leadThinking`,按 run_id 过滤,**不**调 `runs.get`(避免每 token 重抓详情);
  done 或 planUpdated 时 active=false。前端对 reasoning 累积用 rAF/节流合并。

- [ ] 在 orchestratorEvents.ts + ipcBridge 加事件(与既有 5 事件同写法);TS 类型完整。
- [ ] 实现 `useLeadThinking`:订阅、run_id 过滤、按 kind 累积(reasoning 追加;phase 入 phaseKeys;text 仅置 heartbeat,**不存内容**),done/卸载/换 run 时重置;节流。
- [ ] `npm run typecheck` = 0 新错。
- [ ] 提交 `feat(orchestrator/ui): leadThinking 线契约 + useLeadThinking 独立订阅钩子`。

---

### Task F2: 会话风格 composer 替换两意图卡 + 乐观跳转

**Files:**
- Create: `ui/src/renderer/pages/orchestrator/OrchestratorComposer.tsx`(共用 chat 风 composer)
- Create: `ui/src/renderer/pages/orchestrator/orchestratorComposer.module.css`(必要时;优先复用既有类名 token)
- Modify: `ui/src/renderer/pages/orchestrator/NewRunIntentBox.tsx`(改用 OrchestratorComposer;createAdhoc 后立即 `?run=id` 跳转)
- Modify: `ui/src/renderer/pages/orchestrator/RunDetail/RunIntentBox.tsx`(改用 OrchestratorComposer;onSend=adjustRun)
- 复用:`ui/src/renderer/hooks/chat/useInputFocusRing.ts`、`components/chat/SendBox/sendbox.css` 的 `.send-button-custom` / `.sendbox-model-btn`、`useModelRange.ts`

**Interfaces:**
- Consumes:`useInputFocusRing`、`useModelRange`、`ipcBridge.orchestrator.runs.{createAdhoc,adjustRun}`、`TCreateAdhocRun`/`TModelRange`。
- Produces:
  ```ts
  interface OrchestratorComposerProps {
    value: string; onChange: (v: string) => void;
    onSubmit: (text: string) => Promise<void>;
    submitting?: boolean; placeholder?: string;
    // 工具栏 pill(空态/新建显示模型范围+自主度;docked 调整态可隐藏高级 pill)
    showModelRange?: boolean; showAutonomy?: boolean;
    modelRange?: ...; onModelRangeChange?: ...; autonomy?: ...; onAutonomyChange?: ...;
    label?: string; // 卡内小标签(如"新建编排"/"调整编排")
  }
  ```
- 外观契约:外 `--bg-2` 包裹 + 内 **rd-24px** 卡(`bg --bg-base`,`border 1px solid var(--color-border-3)`),焦点用
  `useInputFocusRing` 切 border/shadow;`Input.TextArea` 透明无边、autoSize;底部工具栏右侧 = 模型 pill(`.sendbox-model-btn` round small,Brain+label+Down,popover 选 auto/single/range,复用 useModelRange 多模型语义)+ 自主度 pill +
  **圆形发送钮**(Arco `Button shape="circle" type="primary" className="send-button-custom"`,ArrowUp/Send 白填充;disabled 走类默认)。**800px 居中**容器。Enter 发送 / Shift+Enter 换行 / isComposing 守卫。
- 乐观跳转:NewRunIntentBox 提交 createAdhoc 拿到 run 后**立即**导航到 `?run=<id>`(后端已快返 planning 态)。

- [ ] 实现 OrchestratorComposer(只借类名/hook,**不**引入 ConversationContext/PreviewContext)。
- [ ] NewRunIntentBox 接入(模型范围/自主度 pill;提交→createAdhoc→即时跳转);RunIntentBox 接入(onSend→adjustRun;高级 pill 隐藏)。
- [ ] 圆角/间距/颜色全走 CSS 变量;icon-park 具名导入不起别名;发送用 Arco Button 不裸 `<button>`。
- [ ] `npm run typecheck` = 0 新错。
- [ ] 提交 `feat(orchestrator/ui): 会话风 chat composer 替换意图卡 + 乐观跳转`。

---

### Task F3: RunView 会话风玻璃头 + 上提运行控制

**Files:**
- Modify: `ui/src/renderer/pages/orchestrator/RunDetail/RunView.tsx`(加玻璃头;两视图共用)
- Modify: `ui/src/renderer/pages/orchestrator/RunDetail/DagCanvas.tsx`(把运行控制/标题从这里上提到玻璃头;DagCanvas 仅留画布 + 节点)
- 复用:会话 `chat-layout-header--glass` 样式(类名/视觉),`ChatTitleEditor` 行内重命名模式(可参照其实现,不强行复用其会话耦合)

**Interfaces:**
- Consumes:F1 `useLeadThinking`(玻璃头可显示 active 进度指示)、`useRunLive` 的 `detail`、`ipcBridge.orchestrator.runs.{rename,cancel,approve,pause,resume}`、既有 `ViewToggle`。
- Produces:RunView 顶部统一玻璃头:左 = 运行目标(行内可编辑→rename)+ 状态 pill(STATUS_META 配色);右 = 运行控制按钮组(按状态启用:awaiting→批准、running→暂停/取消、paused→恢复)+ 对话/画布 `ViewToggle`(`headerExtra` 位)。DagCanvas 不再自渲染这些控制(避免双份)。

- [ ] 实现玻璃头(`chat-layout-header--glass` 观感,主题 token);行内重命名(div/input 切换,非裸 button);控制按钮按状态门控。
- [ ] DagCanvas 移除已上提的标题/控制,留画布;确认 cancel/approve/pause/resume 仍可用(经玻璃头)。
- [ ] `npm run typecheck` = 0 新错;手测两视图共用同一头(对话/画布切换头不变)。
- [ ] 提交 `feat(orchestrator/ui): RunView 会话风玻璃头 + 运行控制上提`。

---

### Task F4: 决策流流式思考气泡 + 列表/气泡对齐

**Files:**
- Modify: `ui/src/renderer/pages/orchestrator/RunDetail/RunDecisionFeed.tsx`(顶部插流式「编排思考」气泡;气泡/任务行对齐会话观感)
- Modify: `ui/src/renderer/pages/orchestrator/index.tsx`(`RunListRail` 行对齐会话 session-row 观感)
- 复用:`MessageThinking`/`ThoughtDisplay` 的折叠思考观感(参照其样式,渲染 useLeadThinking 的 reasoning + phaseKeys)

**Interfaces:**
- Consumes:F1 `useLeadThinking`(reasoning/phaseKeys/active/textHeartbeat)、F5 的 i18n 阶段文案 key、既有 `RunDecisionFeed` 的 Robot 头像/气泡样式。
- Produces:
  - 流式思考气泡:lead 头像 + 折叠卡;active 时显示 reasoning(有则)或阶段叙述(phaseKeys→i18n 文案,如「正在拆解目标…」「分派 agent…」「生成计划…」),textHeartbeat 显示「拟稿中…」**不显 JSON**;done/计划就绪后气泡收起为「已完成规划」摘要,下方编排决策卡(既有 TRunDetail 派生)照常浮现。
  - 思考气泡在上(流式)、编排决策卡在下(汇总),autoscroll 既有逻辑兼容。
  - RunListRail 行:头像/图标 + 目标标题 + 状态·时间副标题 + hover 操作,贴合会话 session-row。

- [ ] 渲染流式思考气泡(推理优先,阶段叙述兜底,绝不裸 JSON);接 useLeadThinking。
- [ ] 决策卡/任务行/列表行圆角间距配色对齐会话(全 CSS 变量)。
- [ ] `npm run typecheck` = 0 新错;手测:新建 run 落 planning 态即见思考气泡流动。
- [ ] 提交 `feat(orchestrator/ui): 决策流流式编排思考气泡 + 列表/气泡对齐会话`。

---

### Task F5: i18n 文案(中英对称)+ 类型 + build

**Files:**
- Modify: `ui/src/renderer/services/i18n/locales/zh-CN/orchestrator.json` + `en-US/orchestrator.json`(新增阶段叙述、思考气泡、乐观创建提示、玻璃头控制等文案)
- Modify(生成): `ui/src/renderer/services/i18n/i18n-keys.d.ts`(regen)

**Interfaces:**
- Consumes:F2/F3/F4 使用到的所有新 key。
- Produces:en-US/zh-CN **对称**新增,例如 `run.thinking.title`、`run.thinking.phase.planningStarted/decomposing/assigning/planReady`、`run.thinking.drafting`、`run.thinking.done`、`start.optimistic.*`、`run.header.*`(命名贴合既有 orchestrator.json 结构,实施时核对实际引用 key)。

- [ ] 汇总 F2/F3/F4 引用的新 key,en/zh 对称补齐;无孤儿、无缺失。
- [ ] regen `i18n-keys.d.ts`(`i18n:types` / 既有脚本)。
- [ ] `check:i18n` 绿;`npm run typecheck` = 0;前端 `build` 绿。
- [ ] 提交 `feat(orchestrator/ui): 编排思考/玻璃头/乐观创建 i18n 文案(中英对称)`。

---

## Self-Review / 风险
**覆盖:** 诉求①(对齐)→ F2(composer)+F3(玻璃头)+F4(列表/决策流对齐);诉求②(思考可见+消空挡)→
B1(事件)+B2(流式)+B3(乐观创建+阶段叙述)+B4(adjust 锁外流式)+F1(订阅钩子)+F4(气泡渲染)。
**不变量:** 无 IR/节点图;per-run 锁不跨 LLM await(B4 显式重构 + 测);WS 合并防洪泛;leadThinking 订阅与详情重抓解耦;
sink 默认 None 时后端行为零变化(B2);乐观创建空 run 由 planning 空态承接 + fail-soft;主题全走 CSS 变量。
**风险:** ① adjust 重构动锁路径——测无滞留/死锁 + 语义不变;② WS 洪泛——后端合并+前端节流;③ JSON 裸显——text 仅心跳;
④ 推理不可用——阶段叙述兜底;⑤ 玻璃头与 DagCanvas 控制双份——F3 显式上提去重;⑥ 乐观创建后 FE 处理零任务 run——既有 planning 空态。

## Execution Handoff
SDD:B1→B2→{B3,B4}→F1→F2→F3→F4→F5,每任务新实现者 + 对抗评审,账本 `.superpowers/sdd/progress.md`。
后端 sonnet(B1/B3 机械、B2/B4 标准);前端 frontend-design(标准/opus,视觉门)。最后 whole-branch opus 终评审。
禁 IR / cargo fmt / 合并 main。

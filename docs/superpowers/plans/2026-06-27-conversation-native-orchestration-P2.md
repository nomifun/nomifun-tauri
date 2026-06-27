# P2 — DAG 右栏实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development。Steps `- [ ]`。

**Goal:** 主管(lead)会话的右栏出现一个「编排」tab，渲染该 Run 的 react-flow DAG；点节点看 worker 子会话转录 + 工作状态 + steer。复用既有 DagCanvas/WorkerTranscriptPanel/useRunLive，仅做嵌入接线。**同时修复 P1 引入的回归**：lead 会话因带 `orchestrator_run_id` 被 worker 过滤器误隐藏。

**Architecture:** 在 `ChatSlider` 的 nomi 分支给 `extraTabs` 追加一个「编排」tab（门控 `extra.orchestrator_role==='lead'` 且有 `orchestrator_run_id`）；tab content 内嵌 `DagCanvas`（加 `embedded` 隐藏返回钮）+ 同级挂 `WorkerTranscriptPanel`（local selectedTask 状态）。react-flow 需显式尺寸非滚动容器。

**Spec:** §7（DAG 右栏）、§12 不变量（worker 主侧栏过滤、ReadOnlyConversationView 自挂 PreviewProvider）。

## Global Constraints
- 复用既有组件，**不重写** DagCanvas/TaskNode/layoutDag/WorkerTranscriptPanel/ReadOnlyConversationView/useRunLive。
- 前端 typecheck=0（`cd ui && npm run typecheck`）+ `bun run build` 绿；禁 any/ts-ignore；icon-park 无别名；`<div role=button>` 不用 `<button>`；Arco 弹窗 useArcoMessage。
- worker 会话主侧栏过滤不变量（§12）：worker 仍须隐藏，但 lead 必须可见。
- ReadOnlyConversationView 自挂 PreviewProvider（已修，勿动）。
- 后端禁 cargo fmt；只跑触碰 crate；app 必编过。
- **禁合并 main**；分支 feat/multi-agent-orchestrator。UI 必须漂亮、对齐既有视觉。

## File Structure
- 前端：`ui/src/renderer/pages/conversation/SessionList/hooks/useConversationListSync.ts`（过滤改 task_id）、`ui/src/renderer/pages/conversation/components/ChatSlider.tsx`（加 DAG extraTab）、`ui/src/renderer/pages/orchestrator/RunDetail/DagCanvas.tsx`（加 embedded 隐藏返回钮）、可能新增 `ui/src/renderer/pages/orchestrator/RunDetail/DagRailTab.tsx`（封装 canvas+transcript+state）。
- 后端：`crates/backend/nomifun-db/src/repository/sqlite_conversation.rs`（list 过滤改 task_id）+ 回归测试。

---

## Task 1: 修复 lead 会话被 worker 过滤器误隐藏（前端 + 后端 + 回归）

**Files:** Modify `useConversationListSync.ts`、`sqlite_conversation.rs`；测试 `nomifun-db`。

**根因：** P1 的 `nomi_run_create` 把 `orchestrator_run_id` 写回 lead 会话 extra；而 worker 隐藏过滤器（b8ce2e55 引入）键在 `orchestrator_run_id`：
- 前端 `useConversationListSync.ts:129`：`const isOrchestratorWorkerConversation = !!extra?.orchestrator_run_id;`
- 后端 `sqlite_conversation.rs:779`：`json_extract(c.extra,'$.orchestrator_run_id') IS NULL`

两者会连带把 lead 也隐藏。**worker 与 lead 的区分标记**：worker 由 `build_worker_extra` 同时写 `orchestrator_run_id` + `orchestrator_task_id`；lead 只写 `orchestrator_run_id`（无 task_id）。

**改法：** 过滤器键改为 **`orchestrator_task_id`**（worker 专属标记），lead（无 task_id）保持可见。
- 前端：`const isOrchestratorWorkerConversation = !!extra?.orchestrator_task_id;`
- 后端：`where_parts.push("json_extract(c.extra, '$.orchestrator_task_id') IS NULL".to_string());`

- [ ] **Step 1: 测试（失败优先）** — `nomifun-db` conversation list 测试：种入 (a) 普通会话、(b) lead 会话（extra 仅 `orchestrator_run_id`）、(c) worker 会话（extra 含 `orchestrator_run_id`+`orchestrator_task_id`）；断言 list 返回 (a)+(b)、排除 (c)。先按旧 SQL 跑应 RED（lead 被误排除）。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-db`（新断言失败：lead 当前被隐藏）。
- [ ] **Step 3: 实现** 后端 SQL 改 `orchestrator_task_id`；前端同改。
- [ ] **Step 4: GREEN** `cargo nextest run -p nomifun-db` + `cargo build -p nomifun-db -p nomifun-app`；前端 `cd ui && npm run typecheck`(0)。
- [ ] **Step 5: 提交** `git commit -m "fix(orchestrator): worker 隐藏过滤器改键 orchestrator_task_id(修 lead 会话被误隐藏)"`

---

## Task 2: DagCanvas 加 embedded 模式 + 「编排」extraTab

**Files:** Modify `DagCanvas.tsx`（embedded prop）、`ChatSlider.tsx`（DAG extraTab）；Create `DagRailTab.tsx`。

**契约（已勘察）：**
- `WorkspaceExtraTab { key: string; title: ReactNode; content: ReactNode }`（无 icon/render）。在 `ChatSlider.tsx` nomi 分支（~46-64）的 `extraTabs` 数组追加。content 在 `WorkspaceRailBody:544-546` 被包进 `overflow-y-auto FlexFullContainer` 渲染。
- `DagCanvas` props `{ runId: string; onBack: () => void; onOpenTask: (p: OpenTaskPayload) => void }`，内部 `useRunLive(runId)` 自取数据。含 `RunDetailHeader`（返回钮 + cancel/approve/pause/resume 控件）。
- `WorkerTranscriptPanel` props `{ open: OpenTaskPayload | null; onClose: () => void }`，自带 Arco Drawer。
- `conversation.extra.orchestrator_run_id` 经 cast 读取（TS 类型未声明）；`orchestrator_role?: 'lead'` 已在 create-params extra 类型声明。

**改动：**
1. `DagCanvas`：加可选 `embedded?: boolean`。embedded 时 `RunDetailHeader` 隐藏返回钮（传 `onBack` 给 header 的逻辑改为 embedded→不渲染返回按钮，保留 run 控件）。读 RunDetailHeader 看返回钮如何渲染，加 `hideBack`/`embedded` 透传。
2. 新建 `DagRailTab.tsx`：封装 `{ runId }` → 内部 `useState<OpenTaskPayload|null>(selectedTask)` + 渲染 `<div className="h-full min-h-0 overflow-hidden">`（react-flow 需显式尺寸非滚动父；因 content 外层是 overflow-y-auto，本 div 必须 h-full+min-h-0+overflow-hidden 吃满 rail 高度）包 `<Suspense><DagCanvas runId={runId} embedded onBack={()=>{}} onOpenTask={setSelectedTask}/></Suspense>` + 同级 `<WorkerTranscriptPanel open={selectedTask} onClose={()=>setSelectedTask(null)}/>`。镜像 orchestrator/index.tsx:119/239/250 的组合。
3. `ChatSlider.tsx` nomi 分支：在 extraTabs 数组中，当 `(conversation.extra as {orchestrator_role?:string;orchestrator_run_id?:string})?.orchestrator_role==='lead'` 且 `orchestrator_run_id` 存在时，追加：
   ```tsx
   { key:'orchestrator-dag', title: t('orchestrator.run.dagTab'), content: <DagRailTab runId={orchestrator_run_id}/> }
   ```
   （DagCanvas 已 lazy；DagRailTab 可直接 import 或 lazy。）i18n key `orchestrator.run.dagTab`（"编排"/"Orchestration"）双语 + gen:i18n。
4. **react-flow 尺寸验证**：embedded 在 rail 的 overflow-y-auto 容器内，canvas 须吃满高度不塌缩——若 FlexFullContainer 给的是 flex 高度则 h-full 生效；冒烟时确认 canvas 非零高度。

- [ ] **Step 1: 实现** DagCanvas embedded + DagRailTab + ChatSlider extraTab + i18n + gen:i18n。
- [ ] **Step 2: typecheck** `cd ui && npm run typecheck` → 0。
- [ ] **Step 3: build** `cd ui && bun run build` 绿。
- [ ] **Step 4: 提交** `git commit -m "feat(orchestrator): 会话右栏「编排」tab 内嵌 DAG 画布 + 节点转录"`

---

## Task 3: 集成 + 真机冒烟（seed lead+run → 右栏 DAG 渲染 → 点节点）

**Files:** 无（或微调）。验证为主。

- [ ] **Step 1:** 前端 `cd ui && npm run typecheck`(0) + `bun run build`(绿)。`cargo build --workspace` 绿 + `cargo nextest run -p nomifun-db`（Task1 回归）。
- [ ] **Step 2: 真机冒烟（controller）** — `nomifun-web --dist --insecure-no-auth`（临时 NOMIFUN_DATA_DIR target/_p2_smoke，先 free-ports）。直插 DB 种数据：一个 lead 会话（`type=nomi`, extra `{orchestrator_role:'lead', orchestrator_run_id:'run_xxx', workspace:'/x', model_range:{...}}`）+ 一个 `orch_runs`（id=run_xxx, work_dir, fleet_snapshot=2成员, status='running'）+ 2 个 `orch_run_tasks`（其一 conversation_id 指向一个种入的 worker 会话；含 graph_x/y）+ 1 条 dep。打开该 lead 会话 → 右栏出现「编排」tab → 点开渲染 DAG（2 节点 + 边）→ 点一个有 conversation_id 的节点 → WorkerTranscriptPanel Drawer 打开渲染转录。验证：①lead 会话在侧栏可见（Task1 修复）②worker 会话不在侧栏 ③DAG canvas 非零高度渲染 ④点节点转录无 `usePreviewContext` 崩溃 ⑤零 console error ⑥UI 漂亮。截图 target/_p2_smoke。
- [ ] **Step 3: 记账 + 提交**（若有微调）；账本追加 P2 完成行。

## Self-Review（spec §7/§12）
**覆盖：** DAG 右栏 tab→T2；点节点转录→T2（复用 WorkerTranscriptPanel）；lead 可见/worker 隐藏不变量修复→T1；集成冒烟→T3。
**风险：** react-flow 在 overflow-y-auto 容器内的尺寸（T2 Step4 + T3 冒烟兜底）；lead/worker 标记区分（T1 用 orchestrator_task_id）；嵌入返回钮抑制（T2 DagCanvas embedded）。
**复用：** DagCanvas/WorkerTranscriptPanel/useRunLive/ReadOnlyConversationView 零重写。

## Execution Handoff
波次：T1(过滤修复,sonnet——含后端 SQL+回归)→T2(前端嵌入,sonnet)→T3(集成冒烟,opus controller)。每任务两阶评审+fix+记账。禁合并 main。

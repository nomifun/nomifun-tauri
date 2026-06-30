# 多 Agent 智能编排引擎 · P1b 实施计划（Run 前端 + DAG 画布）

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]`.

**Goal:** 让编排「看得见、可交互」：在「智能编排」页里列出/创建 Run，打开一个 Run 进入 **react-flow DAG 画布**（任务节点 + 依赖边 + 实时状态），点节点弹出该 worker 的**实时转录**，全程 WS 实时刷新。纯前端，消费 P1a 后端。

**Architecture:** orchestrator 页用既有 `?section=` 内联态 + 新增 `?run=<id>` 主从态；选中 run 时右栏**绕过居中 max-w-1100px 包裹**，全幅挂 **lazy `@xyflow/react` DagCanvas**（CSS 变量主题 + data-theme MutationObserver）。Run 客户端/WS 订阅扩进既有 `ipcBridge.orchestrator`；节点转录复刻 `SubagentDrawer`（`Drawer` + `TeamChatView hideSendBox`，conversation_id 是 number）。

**Tech Stack:** React 19 + Arco + UnoCSS + SWR + **@xyflow/react（新增依赖）**；HashRouter。

**Spec：** `docs/superpowers/specs/2026-06-26-multi-agent-orchestrator-design.md` §9（前端）。P1a 已交付后端（HEAD `b4c1cb5b`）：run REST（POST /api/orchestrator/runs、GET /workspaces/{ws}/runs、GET /runs/{id}→RunDetail、POST /runs/{id}/cancel）+ WS 事件（orchestrator.run.statusChanged/planUpdated/completed、orchestrator.task.statusChanged/assigned）。前端已有：`ipcBridge.orchestrator.{fleets,workspaces}`、`orchestratorTypes.ts`（TFleet…）、`orchestratorEvents.ts`（5 个手写事件类型，**目前未被消费**）、orchestrator 页（ContentSider 三段 workspace/fleet/run-history，run-history 是 P0 占位）。

## Global Constraints

- **typecheck 归零**：`cd ui && npm run typecheck`（非 npx tsc）；无 vitest，不新增前端单测；改 locale 后 `bun run gen:i18n`（仓库根）+ 同步 i18n-keys.d.ts。
- **禁** `any`/`ts-ignore`/改无关行为；颜色一律 CSS 主题变量/UnoCSS token（禁硬编码 hex）；`@icon-park/react` 具名导入**不起别名**；Arco 弹窗经 `useArcoMessage`；交互元素用 `<div role="button">` 不裸 `<button>`；卡片网格 `minmax(min(Npx,100%),1fr)`。
- **wire 类型 snake_case**、string ids（run_/rtask_/asg_）；`conversation_id`/`lead_conv_id` 是本机 INTEGER → TS `number`（非 string，无 string-gotcha）。
- **WS 事件 string 名必须与后端 `WebSocketMessage.name` 逐字一致**（orchestrator.run.* / orchestrator.task.*）；REST helper 已解包 ApiResponse `.data`，hook 见裸 DTO，勿双解包。
- **canvas 全幅**：DagCanvas 必须**不在** `mx-auto max-w-1100px py-32px` 包裹内；作为 flex-1 pane 直接子节点，`size-full min-h-0`，自己管 overflow（react-flow 需显式定高的非滚动父容器，链路每层 `min-h-0`）。canvas 模式父 pane `overflow-y-auto`→`overflow-hidden`。
- **react-flow**：`bun add @xyflow/react`（ui/ 目录）；`import '@xyflow/react/dist/style.css'`；**lazy 加载**（`React.lazy(() => import('./DagCanvas'))` + `<Suspense fallback={<AppLoader/>}>`，模板 `WebuiControlPanel.tsx` 的 React.lazy）；JS 侧颜色（minimap/marker）用 data-theme MutationObserver（模板 `MermaidBlock.tsx`），节点/边样式优先 CSS 变量。
- **UI 必须漂亮**（硬验收，[[ui-must-be-beautiful]]）：对齐既有视觉语言；orchestrator 内用 `<div role=button bg-primary-6>` 主操作（非 Arco Button）；header `text-18px font-600` + `text-12px text-t-tertiary` 副标题；卡片 `rd-12px bg-1`；走 frontend-design 打磨画布。
- 真机视觉验收：控制台跑（`bun run dev:web` + NOMIFUN_DATA_DIR）；无头 Chrome 截图（puppeteer-core + 系统 Chrome）法见 P0 验收。
- 提交：feature 分支 `feat/multi-agent-orchestrator`；每任务末提交；提交前 `git pull --rebase`。

## File Structure（P1b）

- 修改 `ui/src/common/types/orchestrator/orchestratorTypes.ts` — 加 TRun/TRunTask/TRunTaskDep/TAssignment/TTaskProfile/TRunDetail/TCreateRun。
- 修改 `ui/src/common/adapter/ipcBridge.ts` — `orchestrator.runs.{create,list,get,cancel}` + `orchestrator.runEvents.*` wsEmitters + 类型 import。
- 修改 `ui/src/renderer/pages/orchestrator/useOrchestratorData.ts` — `useRuns(workspace_id)` SWR；创建 `ui/src/renderer/pages/orchestrator/useRunLive.ts`。
- 重写 `ui/src/renderer/pages/orchestrator/RunHistory.tsx` — 真 run 列表 + 「新建 Run」入口。
- 创建 `ui/src/renderer/pages/orchestrator/CreateRunModal.tsx` — workspace+fleet+goal+autonomy。
- 创建 `ui/src/renderer/pages/orchestrator/RunDetail/DagCanvas.tsx` + `nodes/TaskNode.tsx` + `RunDetail/RunDetailHeader.tsx` + `RunDetail/WorkerTranscriptPanel.tsx`。
- 修改 `ui/src/renderer/pages/orchestrator/index.tsx` — `?run=` 主从态 + 全幅 canvas 分支 + lazy DagCanvas。
- 修改 `ui/src/renderer/services/i18n/locales/{zh-CN,en-US}/orchestrator.json` + i18n-keys.d.ts。
- 修改 `ui/package.json`（+@xyflow/react）。

---

## Task 1: Run 类型 + ipcBridge runs 客户端 + WS emitters

**Files:** Modify `orchestratorTypes.ts`、`ipcBridge.ts`；Test: typecheck。

**Interfaces produced（照搬 extraction notes 的精确形态）:**
```ts
// orchestratorTypes.ts（snake_case，string ids，i64→number，Option→?，Vec<String>→string[] 必填）
export type TTaskProfile = { kind: string; needs_vision: boolean; needs_long_context: boolean; needs_high_reasoning: boolean; bulk: boolean };
export type TRun = { id: string; workspace_id: string; goal: string; autonomy: string; max_parallel?: number; status: string; summary?: string; lead_conv_id?: number; total_tokens?: number; created_at: number; updated_at: number };
export type TRunTask = { id: string; run_id: string; title: string; spec: string; task_profile?: TTaskProfile; status: string; conversation_id?: number; output_summary?: string; output_files: string[]; attempt: number; tokens?: number; graph_x?: number; graph_y?: number };
export type TRunTaskDep = { blocker_task_id: string; blocked_task_id: string };
export type TAssignment = { id: string; task_id: string; member_id: string; score?: number; rationale?: string; source: string; locked: boolean };
export type TRunDetail = { run: TRun; tasks: TRunTask[]; deps: TRunTaskDep[]; assignments: TAssignment[] };
export type TCreateRun = { workspace_id: string; goal: string; fleet_id: string; autonomy?: string; max_parallel?: number };
```
```ts
// ipcBridge.ts — inside the EXISTING `orchestrator` object (sibling of fleets/workspaces); httpGet/httpPost already unwrap .data
runs: {
  create: httpPost<TRun, TCreateRun>('/api/orchestrator/runs'),
  list: httpGet<TRun[], { workspace_id: string }>((p) => `/api/orchestrator/workspaces/${p.workspace_id}/runs`),
  get: httpGet<TRunDetail, { id: string }>((p) => `/api/orchestrator/runs/${p.id}`),
  cancel: httpPost<void, { id: string }>((p) => `/api/orchestrator/runs/${p.id}/cancel`),
},
runEvents: {
  statusChanged: wsEmitter<TOrchRunStatusEvent>('orchestrator.run.statusChanged'),
  planUpdated:   wsEmitter<TOrchRunPlanUpdatedEvent>('orchestrator.run.planUpdated'),
  completed:     wsEmitter<TOrchRunCompletedEvent>('orchestrator.run.completed'),
  taskStatusChanged: wsEmitter<TOrchTaskStatusEvent>('orchestrator.task.statusChanged'),
  taskAssigned:      wsEmitter<TOrchTaskAssignedEvent>('orchestrator.task.assigned'),
},
```
- Consumes: existing `httpGet/httpPost/wsEmitter` from `./httpBridge`；`orchestratorEvents.ts` 的 5 个事件类型（import them）；`get` 返回 `TRunDetail`（非 TRun）；`cancel` 无 body 返 void（若 httpPost 要 body builder 用 `() => undefined`，参照 channel.disablePlugin 无 body idiom）。

参照模板：ipcBridge.ts 既有 `orchestrator` 块（fleets/workspaces，~line 2726）；cron wsEmitter 注册（~line 1440）；`orchestratorEvents.ts`（事件名逐字）。

- [ ] **Step 1: 加 TS 类型** 到 orchestratorTypes.ts（确认 orchestratorEvents.ts 的 5 个类型名：TOrchRunStatusEvent/TOrchRunPlanUpdatedEvent/TOrchRunCompletedEvent/TOrchTaskStatusEvent/TOrchTaskAssignedEvent；以文件实际为准）。
- [ ] **Step 2: 加 runs + runEvents** 到 ipcBridge.orchestrator + 类型 import 块（ipcBridge.ts:87-94 区域）。
- [ ] **Step 3: typecheck** `cd ui && npm run typecheck` → 0。
- [ ] **Step 4: 提交** `git commit -m "feat(orchestrator): 前端 run 客户端 + WS 订阅 + 类型"`

---

## Task 2: useRuns SWR + useRunLive hook

**Files:** Modify `useOrchestratorData.ts`（加 `useRuns(workspace_id)`）；Create `useRunLive.ts`；Test: typecheck。

**Interfaces produced:**
```ts
// useOrchestratorData.ts
export function useRuns(workspaceId: string | undefined): { runs: TRun[]; isLoading: boolean; error: unknown; mutate: () => void }; // SWR key `orchestrator/runs/${workspaceId}`, fetcher ipcBridge.orchestrator.runs.list, revalidateOnFocus:false
// useRunLive.ts
export function useRunLive(runId: string | undefined): { detail: TRunDetail | null; loading: boolean; refetch: () => Promise<void> };
```
- `useRunLive`：初始 `ipcBridge.orchestrator.runs.get.invoke({id})`；useEffect 订阅全部 5 个 `ipcBridge.orchestrator.runEvents.*`，回调 `if (e.run_id === runId) refetch()`，cleanup 调用每个 `.on()` 返回的 unsubscribe。（可选优化：用 event.status 做乐观就地 patch，先做简单 refetch 版。）

参照模板：`useCronJobs.ts` 的 `useCronJobRuns`（per-id REST+订阅）；`useTeamSession.ts`（SWR+多 .on() unsub）；useOrchestratorData.ts 既有 useFleets/useWorkspaces。

- [ ] **Step 1: 写 useRuns**（仿 useFleets/useWorkspaces SWR）。
- [ ] **Step 2: 写 useRunLive.ts**（订阅 5 事件 filter run_id refetch；NomiFun Apache header）。
- [ ] **Step 3: typecheck** → 0。
- [ ] **Step 4: 提交** `git commit -m "feat(orchestrator): useRuns + useRunLive 实时 hook"`

---

## Task 3: RunHistory 真列表 + 新建 Run

**Files:** Rewrite `RunHistory.tsx`；Create `CreateRunModal.tsx`；i18n orchestrator.run.*；Test: typecheck。

**行为:**
- RunHistory：取 workspaces（useWorkspaces）→ 对每个 workspace useRuns → 合并按 created_at desc 列出（或先做「选一个 workspace 看其 runs」+ 默认首个，**取简单可用**：workspace 下拉 + 该 workspace 的 run 列表）。每行 = goal 截断 + status 徽标 + 时间 + 点击 `onOpenRun(run.id)`（page 注入，setSearchParams `{run:id}`）。空态 + 「新建 Run」。
- CreateRunModal（NomiModal）：workspace select（useWorkspaces）+ fleet select（useFleets）+ goal textarea + autonomy select（自主/守护/协同，默认守护）→ `ipcBridge.orchestrator.runs.create.invoke({workspace_id,goal,fleet_id,autonomy})` → `mutate` + `useArcoMessage` 成功 → 回调 onCreated(run.id) → page 打开该 run。校验 goal 非空、workspace+fleet 必选。
- i18n：orchestrator.run.{title,emptyTitle,emptyDesc,newRun,goal,workspace,fleet,autonomy,autonomyAutonomous,autonomySupervised,autonomyInteractive,status.*} 双语 + gen:i18n。

参照模板：`WorkspaceList.tsx`（list+create modal 骨架、SWR、role=button 主操作、卡片 rd-12px bg-1）；P0 FleetManager/FleetEditDrawer（select 模式）。

- [ ] **Step 1: i18n keys 双语 + gen:i18n**。
- [ ] **Step 2: CreateRunModal.tsx**（workspace/fleet/goal/autonomy → create → onCreated）。
- [ ] **Step 3: RunHistory.tsx 重写**（workspace 选择 + run 列表 + 新建入口 + onOpenRun prop + 空/loading/error）。
- [ ] **Step 4: typecheck** → 0。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): Run 列表 + 新建 Run"`

---

## Task 4: DagCanvas（react-flow DAG 画布）

**Files:** Modify `ui/package.json`（+@xyflow/react）；Create `RunDetail/DagCanvas.tsx`、`RunDetail/nodes/TaskNode.tsx`、`RunDetail/RunDetailHeader.tsx`；Test: typecheck + 真机渲染。

**行为:**
- `bun add @xyflow/react`（ui/ 目录）；`import { ReactFlow, Background, Controls, MiniMap, type Node, type Edge } from '@xyflow/react'` + `import '@xyflow/react/dist/style.css'`。
- DagCanvas(props: `{ runId: string; onBack: () => void; onOpenTask: (task: TRunTask) => void }`)：`const { detail } = useRunLive(runId)`；map `detail.tasks` → react-flow `Node[]`（自定义 `TaskNode`：title、status 色点、分派 agent/model chip（从 assignments→member）、进度/重试），`detail.deps` → `Edge[]`（blocker→blocked，animated when 下游 running）；加一个顶部「主管」节点（可选，先省略，聚焦任务图）。
- **布局**：若 task 有 graph_x/graph_y 用之；否则简单分层自动布局（按依赖深度计算 layer，同层横向铺开）——写一个纯函数 `layoutDag(tasks, deps) -> Record<id,{x,y}>`（拓扑分层；可单测纯函数）。
- **主题**：节点/边样式用 CSS 变量（node bg `var(--bg-2)`、border `var(--border-base)`、text `var(--text-primary)`、status 色 `var(--success/warning/danger)`、running 高亮 `var(--primary)`）；ReactFlow `colorMode` / MiniMap/Controls 的 JS 侧色用 data-theme MutationObserver（模板 MermaidBlock）。
- **全幅**：根 `className="size-full min-h-0 flex flex-col"`；ReactFlow 容器 `flex-1 min-h-0`。
- `RunDetailHeader`：goal、status 徽标、聚合进度（done/total）、cancel 按钮（confirm）→ `ipcBridge.orchestrator.runs.cancel`、返回按钮 onBack。
- 点节点 → `onOpenTask(task)`（Task 5 的转录面板）。

参照模板：`WebuiControlPanel.tsx`（React.lazy）；`MermaidBlock.tsx`（data-theme observer）；CSS 变量来自 default-color-scheme.css。@xyflow/react 文档语义（ReactFlow/Node/Edge/Background/Controls/MiniMap）。

- [ ] **Step 1: 装依赖** `cd ui && bun add @xyflow/react`（确认进 package.json+lock）。
- [ ] **Step 2: layoutDag 纯函数 + 拓扑分层**（可在文件内 #[cfg]-style 不适用前端；至少结构清晰，typecheck 覆盖）。
- [ ] **Step 3: TaskNode.tsx**（自定义节点，状态色、agent/model chip，role=button，主题变量）。
- [ ] **Step 4: RunDetailHeader.tsx**（goal/status/进度/cancel/back）。
- [ ] **Step 5: DagCanvas.tsx**（useRunLive → nodes/edges → ReactFlow + Background/Controls/MiniMap，data-theme observer，全幅）。
- [ ] **Step 6: typecheck** → 0；`cd ui && bun run build`（确认 react-flow 打包无误）。
- [ ] **Step 7: 提交** `git commit -m "feat(orchestrator): DAG 画布(react-flow)"`

---

## Task 5: 节点 worker 转录面板

**Files:** Create `RunDetail/WorkerTranscriptPanel.tsx`；Test: typecheck。

**行为:** 复刻 `SubagentDrawer.tsx`：Arco `<Drawer width={560} footer={null} onCancel>`；按 `task.conversation_id`（number，无 string 转换）`ipcBridge.conversation.get.invoke({id})` 取 `TChatConversation`（cancelled-flag effect 兜底）；body `<TeamChatView conversation={conv} hideSendBox agent_name={task.title} />`（`hideSendBox` 即只读开关；不传 team_id）。无 conversation_id（任务未跑）时显示「该任务尚未开始/无对话」。

参照模板：`pages/conversation/components/multiAgent/SubagentDrawer.tsx` + `TeamChatView.tsx`；只读复用见 extraction notes（NomiChat 自挂 provider + 合并实时流，conversation_id 是 number）。

- [ ] **Step 1: WorkerTranscriptPanel.tsx**（Drawer + TeamChatView hideSendBox + load-by-id + 无对话兜底）。
- [ ] **Step 2: typecheck** → 0。
- [ ] **Step 3: 提交** `git commit -m "feat(orchestrator): 节点 worker 实时转录面板"`

---

## Task 6: orchestrator 页主从接线 + i18n + 真机验收

**Files:** Modify `orchestrator/index.tsx`；i18n 收尾；Test: typecheck + 真机截图。

**行为:**
- index.tsx：`const DagCanvas = React.lazy(() => import('./RunDetail/DagCanvas'))`（模块顶层）；加 `selectedRunId` 由 `searchParams.get('run')` 派生，open/close 经 setSearchParams（open replace:false 让浏览器后退关闭，参照 WorkspacePage 的 `?req=`）。
- flex-1 pane 改条件渲染：`selectedRunId` 时 `overflow-hidden` 全幅 `<Suspense fallback={<AppLoader/>}><DagCanvas runId={selectedRunId} onBack={closeDetail} onOpenTask={setTaskPanel}/></Suspense>`（**不**在 mx-auto/max-w-1100px 包裹内）；否则原居中 content。
- RunHistory 的 onOpenRun → `setSearchParams({section:'run-history', run:id})`。
- WorkerTranscriptPanel 由 DagCanvas 的 onOpenTask 驱动（state 在 index 或 DagCanvas 内，取简单）。
- 移动端：canvas 体验差 → 选 run 时显示只读 run 摘要（status+任务列表）而非 DAG（index.tsx 移动分支）。
- i18n 收尾 + gen:i18n + i18n-keys.d.ts。

参照模板：WorkspacePage 的 `?req=` 主从态；orchestrator/index.tsx 既有 pane 结构（居中包裹在 ~line 169，须条件绕过）。

- [ ] **Step 1: index.tsx 主从态 + 全幅 canvas 分支 + lazy DagCanvas + 移动兜底**。
- [ ] **Step 2: RunHistory onOpenRun 接线**。
- [ ] **Step 3: typecheck** → 0；`cd ui && bun run build` 绿。
- [ ] **Step 4: 真机验收** — `bun run dev:web`（NOMIFUN_DATA_DIR 临时目录）+ 无头 Chrome 截图：进「智能编排」→ run-history → 新建 Run（选 workspace/fleet/填 goal）→ 列表出现 → 打开 → DAG 画布渲染（即便无 provider 致 plan 出错，画布壳+空态/错误态应优雅渲染；若配了 provider 则见任务节点）。截图留证（画布 + 列表 + 新建弹窗）。零 console error。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): Run 主从页接线 + DAG 画布集成 + 真机验收"`

---

## Self-Review（对照 spec §9 的 P1b 切片）

**Spec 覆盖：** §9 DAG 编排画布(react-flow) → Task 4；点节点展开 agent 实时对话 → Task 5；WS 实时刷新 → Task 1/2；run 列表/创建 → Task 3；主从页接线 → Task 6。**P1b 不含**：能力 Router 可视化打分(P3)、自主级别交互闸(P3)、并行(P2)、team 移除(P5)、画布手动改派/锁定 UI(P3 分派交互；P1b 先只读展示分派)。

**占位符扫描：** 无 TBD；「主管节点先省略」「乐观 patch 先做 refetch 版」「移动端只读摘要」是有意的 P1b 范围边界（注明）。代码步给真实类型/recipe/模板路径。

**类型一致：** TRun/TRunDetail（Task1）→ useRunLive（Task2）→ DagCanvas（Task4）一致；conversation_id number（Task1/5）；事件名逐字（Task1）；ipcBridge.orchestrator.runs/runEvents（Task1）→ hooks（Task2）→ UI（Task3/4）一致。

## Execution Handoff

依赖波次：Task1→Task2；Task1→Task3；Task4 依赖 1+2；Task5 依赖 1；Task6 依赖 3+4+5。
执行：**subagent-driven-development**，每任务 implementer→两阶评审→fix loop→记账。**Task4（DAG 画布）用 opus**（UI 重头 + react-flow 新依赖 + 美感硬门槛，走 frontend-design 精神）；其余 sonnet。真机视觉验收由 controller 在 Task6 后做（同 P0）。

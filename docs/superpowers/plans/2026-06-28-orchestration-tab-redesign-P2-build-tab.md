# P2 — 智能编排 Tab 重设计：建 Tab 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development。Steps `- [ ]`。（经 Workflow 执行。）

**Goal:** 把「智能编排」Tab 从只读 Run 历史库升级为完整编排主页：结构化「新建 Run」表单快速发起 → 落到 Run 主视图（头部状态+控制 · agent 花名册 · DAG 画布 · 点节点出 inspector · 完成沉淀）。全程在 Tab 内。

**Architecture:** master-detail：左 Run 列表 + ＋新建 Run 表单入口；主区 = 选中 Run 视图。复用 DagCanvas/RunDetailHeader/WorkerTranscriptPanel/RolePrecipitationPanel/useRunLive/useOrchestratorData(listMine)；新增 NewRunComposer + AgentRoster。经 `ipcBridge.orchestrator.runs.createAdhoc`(P1) 直建 run。**先建后拆**：本期会话融合胶水仍在,但 Tab 已自洽;P3 再拆。

**Tech Stack:** React19/Arco/UnoCSS/react-flow/SWR。

## Global Constraints
- 复用既有组件,**不重写** DagCanvas/RunDetailHeader/WorkerTranscriptPanel(inspector)/RolePrecipitationPanel/useRunLive。
- 前端 typecheck0(`cd ui && npm run typecheck`,非 npx tsc)+`bun run build` 绿;禁 any/ts-ignore;icon-park 具名无别名;`<div role=button>` 不裸 `<button>`;Arco 弹窗 useArcoMessage;CSS 主题变量。**UI 必须漂亮**(硬门槛,用户严格,active/状态色禁刺眼大色块,参照既有视觉)。
- **auto 客户端展开**:REST /runs/adhoc 不展开 `{mode:'auto'}`(create_adhoc 对 auto 返 BadRequest)。NewRunComposer 选「自动」时,用 useModelProviderList 把全部启用模型展开成 `{mode:'range',models:[...]}` 再 POST(或 single 当只有一个)。
- 不改后端;不动会话融合胶水(P3 拆)。**禁合并 main**。分支 feat/multi-agent-orchestrator。

## File Structure
- Create `ui/src/renderer/pages/orchestrator/NewRunComposer.tsx`（表单）
- Create `ui/src/renderer/pages/orchestrator/RunDetail/AgentRoster.tsx`（花名册条）
- Modify `ui/src/renderer/pages/orchestrator/index.tsx`（master-detail 组装 + 新建入口 + 选中 run 视图）
- 复用：`RunDetail/DagCanvas.tsx`、`RunDetailHeader.tsx`、`WorkerTranscriptPanel.tsx`、`RolePrecipitationPanel.tsx`、`useRunLive.ts`、`useOrchestratorData.ts`(useMyRuns/listMine)、`RunHistory.tsx`
- i18n `locales/{zh-CN,en-US}/orchestrator.json`

---

## Task 1: NewRunComposer（结构化发起表单）

**Files:** Create `NewRunComposer.tsx`；i18n。

**Interfaces:**
- Produces: `<NewRunComposer onCreated={(runId:string)=>void} onCancel={()=>void} />`。内部 `ipcBridge.orchestrator.runs.createAdhoc.invoke(req)` → `onCreated(run.id)`。
- Consumes: `ipcBridge.orchestrator.runs.createAdhoc`(P1)、`useModelProviderList`(模型列表)、`ipcBridge.assistants.list`(角色钉选)、`ipcBridge.dialog.showOpen`(工作路径,仿 GuidWorkspaceFootnote)。

**字段：**
- 工作路径(可空)：文件夹选择(ipcBridge.dialog.showOpen)+ 最近路径(可选)。
- 需求：`Input.TextArea`(必填)。
- 模型范围：分段「单一/自动/范围」(用既有 SegmentedTabs 或 Arco Radio.Group type=button——**对齐既有精致风,勿用刺眼满色**)。单一=单选模型;自动=提示"全部启用模型"(提交时客户端展开为 range);范围=多选模型。
- 角色(可选)：多选启用助手(assistants.list,enabled)作钉选角色池;空=自动。
- 自主级别：分段「先审批(interactive,默认)/直跑(supervised)」。
- 提交：构 `TCreateAdhocRun{ goal:需求, work_dir:路径||undefined, model_range:(auto→展开range), pinned_roles:选中助手id, autonomy }` → createAdhoc → onCreated(run.id);失败 useArcoMessage 报错。需求空/范围空 guard。

- [ ] **Step 1:** 实现 NewRunComposer(字段 + auto 客户端展开 + createAdhoc + i18n + gen:i18n)。
- [ ] **Step 2:** `cd ui && npm run typecheck` → 0。
- [ ] **Step 3:** `cd ui && bun run build` 绿。
- [ ] **Step 4:** `git commit -m "feat(orchestrator): 新建 Run 结构化表单 NewRunComposer"`

---

## Task 2: AgentRoster（agent 花名册条）

**Files:** Create `RunDetail/AgentRoster.tsx`；i18n。

**Interfaces:**
- Produces: `<AgentRoster detail={TRunDetail} selectedTaskId={string|null} onSelectTask={(p:OpenTaskPayload)=>void} />`。
- Consumes: `TRunDetail`(run/tasks/deps/assignments/fleet_members)、`OpenTaskPayload`(DagCanvas 导出)、`taskStatusMeta`(TaskNode 导出,状态色)。

**行为：** 横向条,每任务一张小卡：角色(task.role??member.role_hint)·模型(member.model)·状态点(taskStatusMeta 色)。点卡 → 构 OpenTaskPayload(task+assignment+fleetMembers+runId+refetch)→ onSelectTask(同 DAG 点节点,打开 inspector)。member 经 assignment.member_id→fleet_members.find 解析(null-safe)。选中态高亮。漂亮、紧凑、主题变量。

- [ ] **Step 1:** 实现 AgentRoster(聚合 + 点选 + 状态色复用 + i18n)。
- [ ] **Step 2:** typecheck0 + build 绿。
- [ ] **Step 3:** `git commit -m "feat(orchestrator): agent 花名册条 AgentRoster(角色·模型·状态)"`

---

## Task 3: index.tsx master-detail 组装

**Files:** Modify `index.tsx`；可能微调 RunHistory(作左栏列表)；i18n。

**布局：** 左 = Run 列表(复用 useMyRuns/listMine,active+历史,＋新建 Run 按钮);主 = 若新建态→`<NewRunComposer onCreated={runId=>选中该 run}/>`;若选中 run→Run 视图：`<RunDetailHeader>`(状态+控制,复用) + `<AgentRoster detail onSelectTask={setSelectedTask}/>` + `<DagCanvas runId embedded? onOpenTask={setSelectedTask}/>`(主区全幅,非折叠栏,故 embedded 隐返回钮 + fitView 正常) + `<WorkerTranscriptPanel open={selectedTask} onClose/>`(inspector) + run 完成时 `<RolePrecipitationPanel detail/>`。状态 `selectedRunId` + `selectedTask` + `composing` 三态。
- DagCanvas 在 Tab 主区是大尺寸容器(非窄栏),fitView 应正常;embedded 用于隐藏返回钮(主区导航用左栏列表)。或新增 prop 控制。
- useRunLive(selectedRunId) 取 detail 喂 AgentRoster/RolePrecipitationPanel(DagCanvas 自取)。
- 移动端:沿用 MobileRunSummary 或简化(保持不崩)。
- 路由 `?run=`/`?new` 同步(可选,沿用现有 ?run= takeover)。

- [ ] **Step 1:** 组装 master-detail(列表+新建+run视图);三态切换;selectedTask/selectedRunId 线。
- [ ] **Step 2:** typecheck0 + build 绿。
- [ ] **Step 3:** `git commit -m "feat(orchestrator): 智能编排 Tab master-detail(列表+新建表单+Run主视图)"`

---

## Self-Review（spec §3/§4）
**覆盖:** 快速发起→T1(表单);花名册可见→T2;主视图组装(头部/控制/DAG/inspector/沉淀)→T3。配置(模型范围/角色钉选)→T1;逐 agent 配置/控制→复用 inspector(T3 接)。
**复用:** DagCanvas/RunDetailHeader/WorkerTranscriptPanel/RolePrecipitationPanel/useRunLive 零重写。
**风险:** auto 客户端展开(T1,REST 不收 auto);DagCanvas 主区 fitView/embedded(T3,大容器应正常,P4 冒烟兜底);UI 美观(T1/T2 用既有精致控件,P4 controller 截图判)。
**类型:** TCreateAdhocRun(P1)↔composer;OpenTaskPayload/TRunDetail/taskStatusMeta 复用既有导出。

## Execution Handoff
Workflow:understand(2 并行:现 index.tsx/RunHistory/useMyRuns + composer 依赖 useModelProviderList/assistants.list/dialog)→implement(T1 composer + T2 roster 可并行;T3 组装串行)→verify(对抗:auto展开/类型一致/复用正确/hygiene)。P4 真机冒烟+截图判美。禁合并 main。

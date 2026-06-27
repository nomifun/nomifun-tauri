# P5 — 沉淀 + 退役 + 资产库实施计划（终章）

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development。Steps `- [ ]`。

**Goal:** ①**沉淀**：规划为每个任务命名角色；Run 完成后把用到的角色"建议保存"为可复用助手（一键采纳）。②**退役**：删除 workspace/fleet/新建Run 旧创建 UI，把「智能编排」tab 收敛为只读 **Run 历史库**（需 list-runs-by-user，因 adhoc run 的 workspace_id=NULL）。③**最终全分支评审**。

**Architecture:** 规划 PlannedTask 加 `role` + 持久化 orch_run_tasks.role(迁移022);DagCanvas 完成态显示沉淀候选(按 role 聚合 TRunDetail,经 ipcBridge.assistants.create 保存);orchestrator tab 三段壳→单一只读 Run 历史(新增 list_runs_by_user 路由 + runs.listMine)。退役删除 Fleet*/Workspace*/CreateRunModal,保留整个 RunDetail/* + run 控件 + runEvents。

**Spec:** §5（沉淀）、§10（退役+资产库）、§8（caps 保留）、§12-15。

## Global Constraints
- 保留 RunDetail/*（DagCanvas/DagRailTab/RunDetailHeader/TaskNode/WorkerTranscriptPanel/ReadOnlyConversationView/MobileRunSummary/useRunLive/layoutDag/memberLabel）+ run 控件(cancel/approve/pause/resume/reassign/steer)+ runEvents——会话右栏也用它们。
- 删除 dead SWR(useFleets/useWorkspaces)与 dead ipcBridge create/update/delete **必须与 UI 移除原子完成**，否则编排页 throw。
- 迁移 append-only 022。后端禁 cargo fmt；只跑触碰 crate；app 必编过。前端 typecheck0+build；禁 any/ts-ignore;icon-park 无别名;`<div role=button>`;Arco useArcoMessage。
- 不破坏 orchestrator_run_e2e(4/4)/run 生命周期/IDMM/P1-P4。**禁合并 main**。UI 必须漂亮。
- 预设(presets)按 §15 **降级延后**（不做）：资产库=Run 历史(只读) + 助手(既有页)。

## File Structure（已勘察）
- 迁移 `migrations/022_task_role.sql`
- 后端规划/持久化：`nomifun-api-types/src/orchestrator.rs`(PlannedTask.role/RunTask.role)、`nomifun-orchestrator/src/plan.rs`(prompt 要 role)、`run_service.rs`(plan 持久 role)、`repository/orch_run.rs`(CreateTaskParams.role)、`repository/sqlite_orch_run.rs`(INSERT/SELECT)
- 后端 list-by-user：`run_service.rs`(list_by_user)、`repository/orch_run.rs`+`sqlite_orch_run.rs`(list_runs_by_user)、`routes.rs`(GET /api/orchestrator/runs)、`ipcBridge.ts`(runs.listMine)
- 前端沉淀：`pages/orchestrator/RunDetail/DagCanvas.tsx`(完成态候选 banner)、新 `RunDetail/RolePrecipitationPanel.tsx`、`ipcBridge.assistants.create`
- 前端退役：`pages/orchestrator/index.tsx`(收敛单段)、`RunHistory.tsx`(去创建)、删 `WorkspaceList/FleetManager/FleetEditDrawer/FleetCard/FleetMemberRow/fleetConstants/CreateRunModal`、`useOrchestratorData.ts`(删 useFleets/useWorkspaces)、`ipcBridge.ts`(删 dead 编排 create/update/delete)、`orchestratorTypes.ts`(TRun.workspace_id→optional)

---

## Task 1: 规划命名角色 + 持久化（沉淀的捕获）

**Files:** Create `migrations/022_task_role.sql`；Modify `nomifun-api-types/src/orchestrator.rs`、`plan.rs`、`run_service.rs`、`repository/orch_run.rs`、`repository/sqlite_orch_run.rs`；测试内联。

**改动：**
- 迁移：`ALTER TABLE orch_run_tasks ADD COLUMN role TEXT;`
- `PlannedTask` 加 `#[serde(default)] pub role: Option<String>`；`build_plan_user_prompt` 提示要求每个任务给一个简短中文角色名(如"规划"/"前端"/"后端"/"测试"/"设计")并放进输出 JSON 的 `role`。
- `RunTask`/`TRunTask` DTO 加 `role: Option<String>`；`CreateTaskParams` 加 `role`；`run_service.rs` plan 创建任务时持久 `planned.role`；`OrchRunTaskRow` + sqlite INSERT/SELECT 加 role（SELECT * + FromRow,补字段顺序）。
- 兼容：role 可空,旧任务/规划无 role 不报错。

- [ ] **Step 1: 测试（失败优先）** — (a) orch_run_tasks 迁移后含 role 列;(b) PlannedTask 反序列化带 role;(c) plan 持久 role→get_detail 任务带 role;(d) build_plan_user_prompt 要求 role(提示含关键词)。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-db -p nomifun-orchestrator -p nomifun-api-types`。
- [ ] **Step 3: 实现**。
- [ ] **Step 4: GREEN** 上述 + `cargo build -p nomifun-app` + e2e 4/4。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): 规划为任务命名角色 + 持久化(迁移022,沉淀捕获)"`

---

## Task 2: list-runs-by-user（退役的后端前置）

**Files:** Modify `repository/orch_run.rs`、`repository/sqlite_orch_run.rs`、`run_service.rs`、`routes.rs`、`ipcBridge.ts`、`orchestratorTypes.ts`；测试内联。

**改动：**
- `IRunRepository::list_runs_by_user(user_id) -> Vec<OrchRunRow>`，仿 `list_runs`：`SELECT * FROM orch_runs WHERE user_id = ? ORDER BY created_at DESC`。
- `RunService::list_by_user(user_id)`。
- 路由 `GET /api/orchestrator/runs`（用 `Extension<CurrentUser>`，返回该用户全部 run；注意当前该路径只挂 POST，加 GET）。
- `ipcBridge.orchestrator.runs.listMine: httpGet<TRun[], void>('/api/orchestrator/runs')`。
- `TRun.workspace_id` → `workspace_id?: string`（后端是 Option，adhoc run 序列化 null）。

- [ ] **Step 1: 测试（失败优先）** — 种 2 个不同 user 的 run（含 adhoc workspace_id=NULL）→ list_runs_by_user 只返当前 user 的、按时间倒序、含 adhoc。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-db -p nomifun-orchestrator`。
- [ ] **Step 3: 实现** repo+service+route+ipcBridge+类型。
- [ ] **Step 4: GREEN** + `cargo build -p nomifun-app` + 前端 `cd ui && npm run typecheck`(0)。注意公开路由处理器禁 extract Extension<CurrentUser> 致 500——此为受保护路由(有 user),核对挂载层与既有 run 路由一致。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): list-runs-by-user 路由(adhoc run 历史)"`

---

## Task 3: 沉淀 UI — Run 完成建议保存角色为助手

**Files:** Modify `pages/orchestrator/RunDetail/DagCanvas.tsx`；Create `RunDetail/RolePrecipitationPanel.tsx`；i18n。

**改动：** DagCanvas 在 `detail.run.status==='completed'` 时显示沉淀面板（折叠 banner，不挡画布）。`RolePrecipitationPanel({ detail })`：
- 从 `TRunDetail` 聚合候选：按 `task.role`(非空)分组；每组候选 = `{ name: role, description: 该 role 任务标题/spec 摘要, models: 经 assignment.member_id→fleet_member 的 distinct (model), enabled_skills/disabled_builtin_skills: 这些 member 的并集, audience/scenario_tags: 空或从 description 推 }`。
- 跳过已存在同名启用助手（先 `ipcBridge.assistants.list` 比对 name，避免重复沉淀）。
- 每候选一张卡 + 「保存为助手」按钮 → `ipcBridge.assistants.create.invoke(CreateAssistantRequest{ name, description, models, enabled_skills, disabled_builtin_skills, preset_agent_type:'nomi' })` → useArcoMessage 成功提示 + 该卡标记已保存。persona 规则文本本期不回写(用户后续在助手页编辑)——记 carry-forward。
- 无 role 任务的兜底：若全无 role(旧 run)则不显示面板。
- 因 DagCanvas 同时用于会话右栏(DagRailTab)+编排 tab，面板两处都现。视觉对齐既有卡片/CSS 变量。

- [ ] **Step 1: 实现** 聚合 + 面板 + 保存 + 去重 + i18n + gen:i18n。
- [ ] **Step 2: typecheck** `cd ui && npm run typecheck` → 0。
- [ ] **Step 3: build** `cd ui && bun run build` 绿。
- [ ] **Step 4: 提交** `git commit -m "feat(orchestrator): Run 完成沉淀角色为助手(建议保存,一键采纳)"`

---

## Task 4: 退役旧创建 UI + 编排 tab 收敛为 Run 历史库

**Files:** Modify `pages/orchestrator/index.tsx`、`RunHistory.tsx`、`useOrchestratorData.ts`、`ipcBridge.ts`、`orchestratorTypes.ts`；Delete `WorkspaceList.tsx`/`FleetManager.tsx`/`FleetEditDrawer.tsx`/`FleetCard.tsx`/`FleetMemberRow.tsx`/`fleetConstants.ts`/`CreateRunModal.tsx`。

**改动（原子）：**
- `index.tsx`：删 `Section` 三段壳（workspace/fleet）+ 段切换 sider tablist + `?section=`；收敛为单一只读 Run 历史（`RunHistory` 用 `runs.listMine`，无 workspace 选择器、无新建按钮）+ 保留 `?run=` 的 DagCanvas takeover + WorkerTranscriptPanel。点 run 行 → 打开 DagCanvas(只读复盘) 或跳 `lead_conv_id` 会话（二选一：**优先 DagCanvas takeover 复盘**，与既有 `openRun` 一致；额外提供"打开会话"链接跳 `/conversation/{lead_conv_id}`）。
- `RunHistory.tsx`：去 `setCreateOpen` 按钮 + workspace `<Select>` + `<CreateRunModal>`；改用 `runs.listMine` 列出当前用户全部 run（含 adhoc）。
- 删除上列 7 个 Fleet/Workspace/CreateRun 组件文件。
- `useOrchestratorData.ts`：删 `useFleets`/`useWorkspaces`（确认无其它引用——grep；FleetEditDrawer 等已删）。
- `ipcBridge.ts`：删 dead `orchestrator.fleets.{create,update,remove}`、`workspaces.{create,update,remove}`、`runs.create`（grep 确认无引用后删；`fleets.list/get`、`workspaces.list/get` 若彻底无引用一并删，否则留）。保留 `runs.{get,listMine,cancel,approve,pause,resume,reassign,steer}` + `runEvents`。
- `orchestratorTypes.ts`：`TRun.workspace_id?` 已在 T2 改;核对 RunHistory 不依赖 workspace_id 非空。
- 侧栏「智能编排」入口标签可保留（指向 Run 历史库）；i18n section.* dead key 清理 + gen:i18n。

- [ ] **Step 1: 实现** 收敛 index + 改 RunHistory + 删 7 组件 + 清 dead hooks/ipcBridge + 类型。grep 确认无悬挂 import。
- [ ] **Step 2: typecheck** `cd ui && npm run typecheck` → 0（清所有悬挂）。
- [ ] **Step 3: build** `cd ui && bun run build` 绿 + i18n check。
- [ ] **Step 4: 提交** `git commit -m "refactor(orchestrator): 退役 workspace/fleet/新建Run 旧创建 UI,编排 tab 收敛为只读 Run 历史库"`

---

## Task 5: 集成 + 真机冒烟 + 最终全分支评审

- [ ] **Step 1:** `cargo build --workspace` 绿 + `cargo nextest run -p nomifun-orchestrator -p nomifun-db -p nomifun-api-types -p nomifun-gateway -p nomifun-assistant -p nomifun-conversation -p nomifun-app` 全绿（e2e 4/4）；前端 typecheck0+build。grep 全仓确认无悬挂 fleet/workspace 创建引用。
- [ ] **Step 2: 真机冒烟（controller）** — `nomifun-web --dist --insecure-no-auth`（temp target/_p5_smoke）。①编排 tab 仅显 Run 历史(无 workspace/fleet/新建 Run 段)、零 console error；②(seed 一个 completed run+带 role 任务+lead 会话)→ 打开 → 沉淀面板显角色候选 → 点保存 → 助手页/`assistants.list` 出现该助手；③点 run 行复盘 DagCanvas 渲染。截图 target/_p5_smoke。
- [ ] **Step 3: 最终全分支评审（opus，requesting-code-review）** — `scripts/review-package <P1 base cd0e5df2 的前一commit或 merge-base main> HEAD`，覆盖整个会话原生编排 diff(P1-P5);triage 账本 Minor/carry-forward;重点查不变量(§12)未破、退役无悬挂、沉淀/list-by-user 归属。修 Critical/Important（单个 fix subagent 汇总）。
- [ ] **Step 4: 记账 + 综合总结**（交付清单 + carry-forward + 用户验收清单[配 provider 跑真 run/真沉淀]）。

## Self-Review（spec §5/§10）
**覆盖：** 沉淀捕获(role)→T1;沉淀 UI→T3;list-by-user→T2;退役+Run 历史库→T4;最终验证+评审→T5。
**风险：** dead hook/ipcBridge 原子删除(T4 grep+typecheck 兜底);adhoc run 历史需 list-by-user(T2 前置);role 可空兼容旧 run(T1);persona 回写延后(carry-forward);TRun.workspace_id optional 连锁(T2)。
**预设降级**(§15):资产库=Run 历史 + 既有助手页,presets 不做。

## Execution Handoff
波次：T1(规划角色+迁移,opus)→T2(list-by-user,sonnet)→T3(沉淀UI,sonnet)→T4(退役,opus——原子删除跨多文件)→T5(集成+冒烟+**最终全分支评审**,opus controller)。每任务两阶评审+fix+记账。禁合并 main。

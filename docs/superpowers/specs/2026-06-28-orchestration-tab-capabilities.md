# 智能编排 Tab 能力补全（右栏文件/变更 · Run 管理 · 进度节奏 · 全局重编排）

> 在已交付的「智能编排」Tab(HEAD 597bfc14)上补 4 块缺失能力。复用既有基础设施(WorkspaceRailBody 路径栏、引擎、级联 FK)，最小新增。分支 `feat/multi-agent-orchestrator` 就地。

**状态**：设计已与用户对齐。Gap4 定案=**重新规划(原地改目标重分解)**。

## 1. 动机（用户提出的 4 缺口）
1. run 视图缺**右侧文件查看/变更查看**(worker 在 work_dir 产出文件，无处看)。
2. 左侧 Run 列表缺**管理**(标题编辑/删除/收起展开)。
3. 缺**整体进度/节奏**信息。
4. 编排有问题时，用户**无法从全局重新设计编排**。

## 2. Gap 1 — run 右栏：文件 + 变更
**复用**：`WorkspaceRailBody`(源无关、路径根)；`useFileChanges`/`ipcBridge.fileSnapshot.*`(已纯路径键 `{workspace}`)→ Changes 对 `run.work_dir` **零后端**可用；`TerminalWorkspaceRail` 是路径根绑定的现成范式。
**新增**：
- 新建 `pages/orchestrator/RunDetail/RunWorkspaceRail.tsx`：仿 TerminalWorkspaceRail，构造 `WorkspaceSource{ workspace: run.work_dir, tree:{key:run.id, listRoot/listChildren 经新 run 路径浏览路由}, lazyChanges:true }`，渲染 `WorkspaceRailBody`。无 upload/selectFiles/refresh 订阅(只读查看)。
- 后端路径浏览路由：`GET /api/orchestrator/runs/{id}/workspace?path=<rel>` → 受保护层，解析 run 的目录(优先 `run.work_dir`，否则 `orch_workspaces.workspace_dir`)→ 复用 `nomifun_file::list_workspace_level(root, path, search)`(已存在)。ipcBridge `orchestrator.runs.browseWorkspace`。
- `TRun` TS 类型加 `work_dir?: string`(后端 Run DTO 已序列化，仅 TS 缺)。
- run 视图(`index.tsx`)：DagCanvas 所在 detail pane 右侧加**可折叠右栏**(Files/Changes)，绑当前选中 run 的 work_dir;run 无 work_dir(legacy workspace-backed)则栏不显或显空态。

## 3. Gap 2 — Run 列表管理（重命名/删除/收起）
**后端(净新增，皆小)**：
- **删除**：`IRunRepository::delete_run(id)` = `DELETE FROM orch_runs WHERE id=?`(级联 FK 自动删 tasks/deps/assignments，018 已 ON DELETE CASCADE)；`RunService::delete(user_id,id)`(校验归属)；路由 `DELETE /api/orchestrator/runs/{id}`(受保护层，删前 `engine.stop(&id)` 仿 cancel)。ipcBridge `runs.remove`。
- **重命名(改 goal)**：`UpdateRunParams` 加 `goal: Option<String>` + `update_run` SET 分支；`RunService::rename(user_id,id,goal)`；路由 `PATCH /api/orchestrator/runs/{id}`(body `{goal}`)。ipcBridge `runs.rename`。(orch_runs 无 title 列，rename=改 goal。)
**前端**：
- `RunListRow` 加 hover 操作(icon-park outline)：**重命名**(内联 Input 或小弹层改 goal)、**删除**(Popconfirm)。删后若为当前选中 run 则清选中。
- `RunListRail` 加**收起/展开**开关(rail 折叠为窄条/隐藏，复用 `useWorkspaceCollapse` 模式 + 独立 storage key `orchestrator-runlist-collapse`)。

## 4. Gap 3 — 进度 / 节奏
**免费(现有数据)**：run 视图进度区扩展——各状态计数(执行中/待开始/失败/完成)、当前运行任务名、run 耗时(`created_at`→`updated_at`)、run `summary`、token 合计(`run.total_tokens`/`task.tokens`)。`RunDetailHeader` 进度从单 X/Y 升级为状态分解 + 当前任务 + 耗时。
**小新增(列已存在)**：`RunTask` DTO + `TRunTask` 加 `created_at`/`updated_at`(`task_row_to_dto` 补；`OrchRunTaskRow` 已有列)→ 每任务用时/节奏(花名册/inspector 显"用时 Xs"/相对时间)。

## 5. Gap 4 — 全局重新规划（原地改目标重分解）
**后端**：`RunService::replan(user_id, id, edit: ReplanRequest{ goal?, model_range?, autonomy?, pinned_roles? })`：
1. 校验归属 + `engine.stop(&id)`(若在跑)。
2. **清旧计划**：`DELETE FROM orch_run_tasks WHERE run_id=?`(级联删 deps/assignments)——新增 repo `clear_run_tasks(run_id)`。
3. 若 edit 带 goal/model_range/autonomy/pinned_roles：更新 `orch_runs`(goal 经 rename 路径；model_range→重建 fleet_snapshot[与 create_adhoc 同构，Auto 由调用方/前端展开]；autonomy 更新)。
4. 重跑 `plan()`(它本就 append，但现已清空→等价重分解)+ 末尾 autonomy 闸(interactive→awaiting_plan_approval)。
- 路由 `POST /api/orchestrator/runs/{id}/replan`(body ReplanRequest)。ipcBridge `runs.replan`。
- **不改 `plan()` 本身**(create 流仍用其 append 语义)；replan = clear + (edit) + plan，清步骤是新增。
**前端**：run 视图加**「重新规划」**动作 → 打开复用 NewRunComposer 的编辑态(预填该 run 的 goal/model_range/autonomy/pinned_roles)→ 提交 `runs.replan`(auto 客户端展开同 create)→ refetch。适用任意非终态/终态 run(重分解覆盖)。

## 6. 不变量（实施勿破）
- 引擎 `plan()`/Router/worker/engine 调度逻辑**不改**;replan=clear(新)+plan(旧)。delete 靠级联 FK。Gap1 复用 WorkspaceRailBody/list_workspace_level。
- worker 主侧栏隐藏过滤(orchestrator_task_id)保留;会话仍纯单 agent(不回退本次)。
- 新路由全挂受保护 orchestrator_routes 层(带 CurrentUser，归属校验);禁公开层 extract。
- 既有 1883 测试 + orchestrator_run_e2e 4/4 不回归。禁合并 main;禁 cargo fmt;前端 typecheck0+build;UI 漂亮。

## 7. 分期
- **P1 后端 + 类型**：delete_run + rename(update goal) + replan(clear+plan) + RunTask DTO 加 created_at/updated_at + TRun.work_dir + run 路径浏览路由 + ipcBridge(remove/rename/replan/browseWorkspace)。后端测试 + 引擎不回归。
- **P2 前端**：RunWorkspaceRail(右栏 Files/Changes，可折叠) + RunListRow 管理(重命名/删除) + RunListRail 收起 + 进度区扩展(状态分解/当前任务/耗时/summary) + 每任务用时 + 「重新规划」编辑器(复用 composer)。typecheck0+build。
- **P3 集成 + 真机冒烟(desktop-forced + seed run/文件) + 全分支评审**。

## 8. 测试策略
- 后端:delete(级联验证)/rename/replan(clear 后 re-plan 任务数正确、归属、engine.stop)/per-task 时间戳往返/路径浏览路由(受保护层、路径解析)。引擎契约 e2e 4/4。
- 前端:typecheck0+build;真机冒烟(种 run+work_dir 文件):右栏 Files/Changes 显文件;Run 行重命名/删除;进度状态分解;重新规划编辑器预填+提交。controller 截图判美。
- 真 LLM 重分解需 provider，留用户;CI/mock 证 seam。

## 9. Carry-forward
- legacy workspace-backed run(work_dir=None,dir 在 orch_workspaces)右栏:本期取 orch_workspaces.workspace_dir 兜底或空态;
- replan 破坏旧计划(无 fork/历史)——用户选定语义,如需保留历史后续加 fork(forked_from 列已预留)。

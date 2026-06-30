# P1 — 智能编排 Tab 能力补全：后端 + 类型 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development。Steps `- [ ]`。（经 Workflow:understand→implement[free-text]→verify。）

**Goal:** 后端补齐 Run 删除/重命名/重新规划 + 每任务时间戳 + run 路径浏览 + TRun.work_dir，为 P2 前端(右栏文件、管理、进度、重规划)打底。

**Architecture:** 复用引擎+级联 FK+`nomifun_file::list_workspace_level`；新增受保护路由(delete/rename/replan/browseWorkspace)；replan=clear(新)+plan(旧,不改)；per-task 时间戳=DTO 补列(列已存在)。

**Tech Stack:** Rust(axum0.8/sqlx)。

## Global Constraints
- 引擎 `plan()`/Router/worker/engine 调度逻辑**不改**;replan=新 clear 步 + 旧 plan。delete 靠 018 级联 FK(ON DELETE CASCADE)。
- 新路由全挂受保护 `orchestrator_routes`(带 `Extension<CurrentUser>`，按 user_id 归属校验);**禁公开层 extract CurrentUser**。
- 既有 `orchestrator_run_e2e` 4/4 + 触碰 crate 测试不回归。禁 `cargo fmt`;只跑触碰 crate;`nomifun-app` 必编过。
- **禁合并 main**。分支 feat/multi-agent-orchestrator,HEAD 起点 base。

## File Structure
- `nomifun-orchestrator/src/repository/orch_run.rs` + `sqlite_orch_run.rs`(delete_run / clear_run_tasks / UpdateRunParams.goal / task_row_to_dto 时间戳)
- `nomifun-orchestrator/src/run_service.rs`(delete / rename / replan)
- `nomifun-orchestrator/src/routes.rs`(DELETE /runs/{id}、PATCH /runs/{id}、POST /runs/{id}/replan、GET /runs/{id}/workspace)
- `nomifun-api-types/src/orchestrator.rs`(RunTask +created_at/updated_at;ReplanRequest;RenameRequest)
- `ui/src/common/types/orchestrator/orchestratorTypes.ts`(TRun.work_dir? ; TRunTask +created_at/updated_at ; TReplanRequest)
- `ui/src/common/adapter/ipcBridge.ts`(runs.remove/rename/replan/browseWorkspace)

---

## Task 1: Run 删除 + 重命名

**Files:** `orch_run.rs`/`sqlite_orch_run.rs`(delete_run + UpdateRunParams.goal)、`run_service.rs`(delete/rename)、`routes.rs`(DELETE/PATCH /runs/{id})、`api-types/orchestrator.rs`(RenameRequest)、`ipcBridge.ts`。

**Interfaces:**
- `IRunRepository::delete_run(id:&str) -> Result<(), sqlx::Error>` = `DELETE FROM orch_runs WHERE id=?`(级联自动删 tasks/deps/assignments)。
- `UpdateRunParams` 加 `goal: Option<String>`;`update_run` SET 加 `goal` 分支。
- `RunService::delete(user_id,id)`(get_run 校验 user_id 归属→404/403→delete_run);`RunService::rename(user_id,id,goal)`(归属校验→update_run{goal})。
- 路由：`DELETE /api/orchestrator/runs/{id}` → handler `delete_run`(Extension<CurrentUser>;**删前 `engine.stop(&id)`** 仿 cancel_run);`PATCH /api/orchestrator/runs/{id}` → handler `rename_run`(body `RenameRequest{goal:String}`)。挂 orchestrator_routes 受保护层。
- ipcBridge：`runs.remove: httpDelete<void,{id:string}>((p)=>'/api/orchestrator/runs/'+p.id)`;`runs.rename: httpPatch<TRun,{id:string;goal:string}>((p)=>'/api/orchestrator/runs/'+p.id, (p)=>({goal:p.goal}))`。

- [ ] **Step 1: 测试(失败优先)** — delete:种 run+2任务+deps+assignments→delete_run→run 与其 tasks/deps/assignments 全无(级联);跨 user delete→403/404。rename:update goal→get_detail.run.goal 变;跨 user→拒。路由测试仿 list_my_runs 受保护层。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-orchestrator`。
- [ ] **Step 3: 实现** repo delete_run + UpdateRunParams.goal + service delete/rename + 路由(engine.stop on delete) + ipcBridge + RenameRequest DTO。
- [ ] **Step 4: GREEN** nextest + `cargo build -p nomifun-app` + e2e 4/4 + 前端 `cd ui && npm run typecheck`(0)。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): Run 删除(级联)+重命名(改goal)路由 + ipcBridge"`

---

## Task 2: Run 重新规划（清旧计划 + 改目标重分解）

**Files:** `orch_run.rs`/`sqlite_orch_run.rs`(clear_run_tasks)、`run_service.rs`(replan)、`routes.rs`(POST /runs/{id}/replan)、`api-types/orchestrator.rs`(ReplanRequest)、`ipcBridge.ts`。

**Interfaces:**
- `IRunRepository::clear_run_tasks(run_id:&str) -> Result<(), sqlx::Error>` = `DELETE FROM orch_run_tasks WHERE run_id=?`(级联删该 run 的 deps/assignments)。
- `ReplanRequest{ #[serde(default)] goal:Option<String>, #[serde(default)] model_range:Option<ModelRange>, #[serde(default)] autonomy:Option<String>, #[serde(default)] pinned_roles:Vec<String> }`(api-types)。
- `RunService::replan(user_id, id, req:ReplanRequest)`:① get_run 归属校验;② `engine.stop(&id)`(若在跑);③ `clear_run_tasks(id)`;④ 若 req 带字段则更新 orch_runs(goal→update_run{goal};model_range→重建 fleet_snapshot[同 create_adhoc 的 build_members_from_range,Auto 须 caller/前端展开];autonomy→update_run/或 fleet 同步);⑤ `self.plan(id)`(已 append,清空后=重分解)+ plan 末尾 autonomy 闸。
- 路由：`POST /api/orchestrator/runs/{id}/replan`(body ReplanRequest)→ handler `replan_run`(Extension<CurrentUser>)。ipcBridge `runs.replan: httpPost<TRun,{id:string}&TReplanRequest>((p)=>'/api/orchestrator/runs/'+p.id+'/replan', (p)=>{const{id,...b}=p;return b;})`。
- **不改 `plan()`**;replan 的清步是新增。model_range 重建快照逻辑复用 create_adhoc 的 build_members(若可抽共享)。

- [ ] **Step 1: 测试(失败优先)** — replan:种 run(2任务,running)→replan{goal:新}→engine.stop 调用、旧任务清空、按新 goal 重分解(mock planner 产 N 任务)、状态按 autonomy(interactive→awaiting);跨 user→拒;replan 不改 plan() append 语义(plan 单测仍绿)。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-orchestrator`。
- [ ] **Step 3: 实现** clear_run_tasks + replan + 路由 + ReplanRequest + ipcBridge。
- [ ] **Step 4: GREEN** nextest + `cargo build -p nomifun-app` + e2e 4/4。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): Run 重新规划(清旧计划+改目标重分解)路由"`

---

## Task 3: 每任务时间戳 DTO + TRun.work_dir + run 路径浏览路由

**Files:** `api-types/orchestrator.rs`(RunTask +created_at/updated_at)、`run_service.rs`(task_row_to_dto 补;run workspace 解析)、`routes.rs`(GET /runs/{id}/workspace)、`orchestratorTypes.ts`(TRun.work_dir/TRunTask 时间戳)、`ipcBridge.ts`(browseWorkspace)。

**Interfaces:**
- `RunTask` DTO 加 `created_at:i64`、`updated_at:i64`(`#[ts(type="number")]` 若该文件用 ts-rs;勘察—orchestrator.rs 之前确认无 ts-rs,则纯字段);`task_row_to_dto` 从 `OrchRunTaskRow.created_at/updated_at` 透传。`TRunTask` 加 `created_at:number;updated_at:number`。
- `TRun` TS 加 `work_dir?: string`(后端 Run DTO 已序列化 work_dir,仅 TS 补)。
- 路由 `GET /api/orchestrator/runs/{id}/workspace?path=<rel>` → handler `browse_run_workspace`(Extension<CurrentUser>):get_run 归属校验→解析目录(优先 `run.work_dir`;否则若 workspace_id 有则 ws_repo.get(workspace_id).workspace_dir)→ 无目录返空/404→ `nomifun_file::list_workspace_level(root, path, search?)`(复用,签名先勘察)→ Json(结果)。返回结构对齐会话 browse_workspace 的 file-tree 形态(供前端 WorkspaceTreeSource 消费)。
- ipcBridge `orchestrator.runs.browseWorkspace: httpGet<<会话 workspace 同型>,{id:string;path?:string}>((p)=>'/api/orchestrator/runs/'+p.id+'/workspace'+(p.path?('?path='+encodeURIComponent(p.path)):''))`。

- [ ] **Step 1: 测试(失败优先)** — RunTask 往返带 created_at/updated_at(get_detail 任务有时间戳);browse route:种 run(work_dir 指向 temp 目录含文件)→GET /workspace→列出文件;跨 user→拒;无 work_dir→空/404。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-orchestrator`。
- [ ] **Step 3: 实现** RunTask 时间戳 + task_row_to_dto + browse 路由(复用 list_workspace_level) + ipcBridge + TS 类型(work_dir/时间戳)。
- [ ] **Step 4: GREEN** nextest + `cargo build -p nomifun-app` + e2e 4/4 + 前端 typecheck0。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): per-task 时间戳 DTO + TRun.work_dir + run 路径浏览路由"`

---

## Self-Review（spec §2/§3/§4/§5）
**覆盖:** delete/rename→T1;replan→T2;per-task 时间戳(进度节奏)+work_dir(右栏)+ browse 路由(右栏 Files)→T3。Gap1 后端(browse+work_dir)+Gap2 后端(delete/rename)+Gap3 后端(时间戳)+Gap4 后端(replan)全覆盖;Gap1/2/3 前端 UI + Gap4 编辑器→P2。
**不变量:** plan() 不改(replan=clear+plan);级联删;受保护层归属;e2e 4/4。
**类型一致:** ReplanRequest/RenameRequest 后端↔ipcBridge;ModelRange 复用(同 P1-tab 立约);work_dir/时间戳 TRun/TRunTask 对齐后端 Run/RunTask DTO。
**风险:** browse route 返回结构须对齐会话 file-tree(T3 勘察 list_workspace_level + 会话 browse_workspace 返回型);replan 的 model_range 快照重建复用 build_members(Auto 前端展开);engine.stop on delete/replan。

## Execution Handoff
Workflow:understand(map run_service/routes 受保护层 + clear/cascade + list_workspace_level 返回型 + create_adhoc build_members 复用 + ts-rs 有无)→implement(T1→T2→T3 串行,free-text)→verify(对抗:级联/归属受保护层/replan-clear正确-plan不改/引擎不回归/browse 路径解析)。P2 前端;P3 集成冒烟。禁合并 main。

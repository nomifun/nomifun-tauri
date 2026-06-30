# P1 — 智能编排 Tab 重设计：后端适配 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development。Steps `- [ ]`。（本计划经 Workflow 执行：understand→implement→verify。）

**Goal:** 让「智能编排」Tab 能经 REST 直建 run（结构化表单入口），并把 `caps_orchestrator` 适配回显式参数（移除 lead-conversation 概念），为后续建 Tab + 拆会话融合胶水打底。

**Architecture:** 新增受保护 REST `POST /api/orchestrator/runs/adhoc` → 复用 `RunService::create_adhoc`；`caps_orchestrator.nomi_run_create` 改为显式入参（不再读调用会话 extra、不再回写 lead extra）。引擎/Router/worker 逻辑零改。

**Tech Stack:** Rust(axum 0.8/sqlx) 后端；TS ipcBridge。

## Global Constraints
- 引擎只吃 fleet_snapshot、快照驱动；`create_adhoc`/`plan`/Router/engine/worker **逻辑不改**。
- 既有 `orchestrator_run_e2e` 4/4 + 触碰 crate 测试不回归。
- 后端禁 `cargo fmt`；只跑触碰 crate；`nomifun-app` 必编过。
- 受保护路由禁 extract `Extension<CurrentUser>` 于公开层（axum0.8 MissingExtension→500）——挂在与其它 run 路由同一受保护层。
- **禁合并 main**（已反向同步）。worker 仍写 `orchestrator_run_id`+`orchestrator_task_id`；主侧栏隐藏过滤(task_id)保留。
- 分支 `feat/multi-agent-orchestrator`，HEAD 起点见 base。

## File Structure
- `crates/backend/nomifun-orchestrator/src/routes.rs`（+ `POST /runs/adhoc` handler `create_adhoc_run`）
- `crates/backend/nomifun-gateway/src/caps_orchestrator.rs`（nomi_run_create 显式参数 + 去 lead 读写）
- `ui/src/common/adapter/ipcBridge.ts`（`orchestrator.runs.createAdhoc`）
- `ui/src/common/types/orchestrator/orchestratorTypes.ts`（`TCreateAdhocRun` 请求类型，若无）

---

## Task 1: REST `POST /api/orchestrator/runs/adhoc` + ipcBridge

**Files:** Modify `nomifun-orchestrator/src/routes.rs`、`ui/src/common/adapter/ipcBridge.ts`、`ui/src/common/types/orchestrator/orchestratorTypes.ts`；测试内联（路由测试仿 `list_my_runs` 的受保护层测试）。

**Interfaces:**
- Consumes: `RunService::create_adhoc(user_id, CreateAdhocRunRequest{ goal, work_dir:Option<String>, model_range:ModelRange, pinned_roles:Vec<String>, autonomy:Option<String>, max_parallel:Option<i64>, lead_conv_id:Option<i64> })`（已存在，api-types/orchestrator.rs）。
- Produces: `POST /api/orchestrator/runs/adhoc` 受保护路由 → `Json<ApiResponse<Run>>`；`ipcBridge.orchestrator.runs.createAdhoc.invoke(req)`。

**实施要点：**
- handler `create_adhoc_run(State, Extension<CurrentUser>, Json<CreateAdhocRunRequest>)` → `state.run_service.create_adhoc(&user.id, req)` → `ApiResponse::ok(run)`。`lead_conv_id` 由 Tab 不传（None）。挂在 `orchestrator_routes` 同一受保护 Router（与 `list_my_runs`/cancel/approve 同层，带 CurrentUser）。GET `/runs`(listMine) 与 POST `/runs`(create_run 旧) 已在；新增 `/runs/adhoc` POST 不撞。
- 路由创建后是否立即 plan + (interactive)await / (supervised)engine.start？**与 caps 一致**：handler 内 create_adhoc → plan() → 若 run.autonomy=='interactive' 则停在 awaiting_plan_approval（不 start），否则 engine.start。复用 caps 现有编排逻辑(读 caps_orchestrator 现状照搬到 handler，或抽共享函数)。autonomy 缺省 interactive（Tab 默认审批）。
- ipcBridge：`createAdhoc: httpPost<TRun, TCreateAdhocRun>('/api/orchestrator/runs/adhoc')`；`TCreateAdhocRun = { goal:string; work_dir?:string; model_range:{mode:'single';model:{provider_id:string;model:string}}|{mode:'auto'}|{mode:'range';models:{provider_id:string;model:string}[]}; pinned_roles?:string[]; autonomy?:string; max_parallel?:number }`（与后端 ModelRange serde 一致：tag mode/snake_case/provider_id）。

- [ ] **Step 1: 测试(失败优先)** — 路由测试(仿 list_my_runs 受保护层 oneshot)：带 CurrentUser POST /runs/adhoc {goal, model_range:range(2模型)} → 200 + run 创建 + 状态 awaiting_plan_approval(interactive 默认);无 CurrentUser → 非200(受保护)。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-orchestrator`。
- [ ] **Step 3: 实现** handler + 路由挂载 + plan/await/start 编排 + ipcBridge + 类型。
- [ ] **Step 4: GREEN** `cargo nextest run -p nomifun-orchestrator` + `cargo build -p nomifun-app` + `cargo nextest run -p nomifun-app -E 'binary(orchestrator_run_e2e)'`(4/4) + 前端 `cd ui && npm run typecheck`(0)。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): REST /runs/adhoc 受保护路由 + ipcBridge(Tab 表单直建 run)"`

---

## Task 2: caps_orchestrator 适配为显式参数（移除 lead-conversation）

**Files:** Modify `nomifun-gateway/src/caps_orchestrator.rs`；测试内联。

**背景(现状)：** 会话融合期 `nomi_run_create` 改为读调用会话 extra(orchestrator_role/lead/model_range/workspace) + 回写 `orchestrator_run_id` 到 lead 会话。lead-conversation 概念现移除。

**实施要点：**
- `RunCreateParams` 改回显式：`{ goal:String, #[serde(default)] work_dir:Option<String>, #[serde(default)] model_range:Option<ModelRange>, #[serde(default)] autonomy:Option<String> }`。model_range 缺省=Auto(全部启用模型,经 load_provider_summaries 展开,沿用现有 expand_auto_range);work_dir 缺省 None(临时);autonomy 缺省 **supervised**(agent 驱动自动跑——MCP 调用方无 Tab 可审批)。
- handler `create`：不再 `conversation_service.get(ctx.conversation_id)` 读 extra;直接用入参构 `CreateAdhocRunRequest`(lead_conv_id=None)。Auto 展开仍在 caps 层(provider 访问)。create_adhoc → plan → (supervised 默认)engine.start。
- **移除** 回写 `orchestrator_run_id` 到调用会话 extra 的逻辑(lead 概念没了)。
- `nomi_run_status`/`nomi_run_result` 不变(仍 deny_on Remote,保留)。

- [ ] **Step 1: 测试(失败优先)** — caps create(显式 {goal, model_range:range}) → create_adhoc 收到正确参数、run 创建(supervised→running);model_range 缺省 → Auto 展开为启用模型;无 conversation_service.get 调用(不依赖会话 extra)。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-gateway`。
- [ ] **Step 3: 实现** RunCreateParams 显式化 + 去会话 extra 读取 + 去 lead 回写 + 缺省值。
- [ ] **Step 4: GREEN** `cargo nextest run -p nomifun-gateway -p nomifun-orchestrator` + `cargo build -p nomifun-app` + e2e 4/4。
- [ ] **Step 5: 提交** `git commit -m "refactor(orchestrator): caps nomi_run_create 改显式参数(移除 lead-conversation 依赖)"`

---

## Self-Review（spec §5）
**覆盖：** REST /runs/adhoc(Tab 入口)→T1；caps 显式参数 + 去 lead→T2。引擎不改(契约 e2e 4/4 守)。
**类型一致：** TCreateAdhocRun.model_range ↔ 后端 ModelRange serde(mode/snake_case/provider_id) 一致(同 P1 旧实现已立约)。
**占位：** 无。autonomy 缺省:Tab=interactive / caps=supervised(明确)。
**风险：** 受保护层挂载(T1,仿 list_my_runs);Auto 展开留 caps 层(T2);plan/await/start 编排照搬 caps 现逻辑(T1 抽共享或复制)。

## Execution Handoff
经 Workflow 执行：understand(2 并行:路由层+caps 现状)→implement(2 任务串行)→verify(对抗:正确性/受保护层/引擎不回归)。每任务两阶校验。禁合并 main。

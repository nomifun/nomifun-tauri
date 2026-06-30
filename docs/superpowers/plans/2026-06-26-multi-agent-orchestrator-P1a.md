# 多 Agent 智能编排引擎 · P1a 实施计划（Run 引擎后端）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** 让「智能编排」真正跑起来（后端）：一个 Run 从目标 → 主管规划出任务 DAG → 串行调度器在真实 worker 会话上逐个执行 → 产物回流解阻塞 → 聚合完成。纯后端 + gateway 暴露 + 集成测试；前端画布是 P1b。

**Architecture:** 新 crate `nomifun-orchestrator` 扩 Run 引擎。**worker = 真实 nomi 会话**（复用 `nomi_agent_run` 配方：create yolo+desktopGateway → send → await_turn(is_processing) → read_final_text）。**两个可注入 trait 解耦引擎与重依赖**：`PlanProducer`（真实=一次性结构化 LLM 规划调用；测试=mock 固定 DAG）和 `WorkerRunner`（真实=spawn-and-await 配方；测试=mock 产物）。调度器 `RunEngine` 复刻 AutoWork `Orchestrator`（DashMap 按 run_id 注册表 + generation 守卫 + start/stop/run_loop + boot-resume），P1 先**串行**（并行是 P2）。

**Tech Stack:** Rust（axum/sqlx/async-trait/tokio）、SQLite、gateway Registry、EventBroadcaster WS。

**Spec：** `docs/superpowers/specs/2026-06-26-multi-agent-orchestrator-design.md`（§3 状态机 / §4 架构 / §5 数据 / §7 自主 / §8 gateway）。P0 已交付迁移 018 全 7 表 + Row 模型 + Fleet/Workspace 仓库/服务/路由/前端（HEAD 727b3161）。

## Global Constraints

- **迁移 append-only**：P1a **不新增迁移**（018 已建全部 7 表，含 orch_runs/orch_run_tasks/orch_run_task_deps/orch_assignments）。如确需新列 → 新增 `019_*.sql`，绝不改 018。
- **设备边界 ID**：run_/rtask_/asg_ 用 `generate_prefixed_id`；worker `conversation_id` 是本机 `conversations.id` INTEGER（orch_run_tasks.conversation_id 列已是 INTEGER）。
- **仓库模式**：`I*Repository` trait（`repository/{name}.rs`）+ `Sqlite*Repository` impl（`repository/sqlite_{name}.rs`）+ repository/mod.rs 4 行 re-export + nomifun-db/src/lib.rs 根 re-export（P0 已建此惯例，见 lib.rs 既有 fleet/workspace 根导出）；手写 sqlx；TEXT PK 显式 `generate_prefixed_id` 写入。
- **worker = 真实会话**：复用 `ConversationService`/`IWorkerTaskManager`；worker 会话 `extra` 带 `{session_mode:"yolo", desktopGateway:true, workspace, orchestrator_run_id, orchestrator_task_id, system_prompt:<brief>}`；用 `create()`（非 HTTP 路由，extra 不被剥离）。
- **调度器确定性 + 串行**：P1 一次执行一个就绪任务（并行 P2）；引擎复刻 `nomifun-requirement::Orchestrator`（state.rs build_requirement_state + orchestrator.rs 为权威模板）。
- **取消语义**：取消令活跃 worker 以 `Finish(Cancelled)` 收尾（沿用现有协作式取消，勿用 Error）。
- **依赖边表解阻塞**：复用 orch_run_task_deps（completion → check_unblocks）。
- **gateway**：caps_orchestrator 3 步契约（caps_orchestrator.rs register + lib.rs mod + build() 调用）+ GatewayDeps 字段 + routes.rs 装配；工具名 `nomi_` 前缀 ≤42 字符；权限经 DangerTier×Surface 集中 gate（handler 不自查）。
- **服务接线**：`build_orchestrator_run_state` 复刻 `build_requirement_state`（state.rs:769-864）——引擎自建独立 ConversationService（**勿复用** route 实例，那个挂了 user-turn IDMM hook）；deps off `services.*`；boot-resume + sweeper 在 build 时 `tokio::spawn` 分离。
- **测试**：只跑触碰 crate（`cargo nextest run -p <crate>`）；引擎核心用 mock trait 单测，端到端用集成测试（mock agent 或 Mock AgentInstance）；**禁 cargo fmt**；app 必编过（`cargo build -p nomifun-app`）。
- **提交**：feature 分支 `feat/multi-agent-orchestrator`；每任务末提交；提交前 `git pull --rebase`。

## File Structure（P1a）

**后端 — nomifun-db**
- 创建 `repository/orch_run.rs` + `sqlite_orch_run.rs`（`IRunRepository`：runs CRUD + status；tasks CRUD + status/output；deps add/list_ready/check_unblocks；assignments create/list）。或拆 run/task 两文件——见 Task 1 决策。
- 修改 `repository/mod.rs` + `src/lib.rs`（re-export）。

**后端 — nomifun-api-types**
- 创建/扩展 `src/orchestrator.rs`：`Run`、`RunTask`、`RunTaskDep`、`Assignment`、`RunDetail`(run+tasks+deps+assignments)、`CreateRunRequest`、`PlannedDag`/`PlannedTask`（规划产物）、`RunStatus`/`TaskStatus` 枚举。

**后端 — nomifun-orchestrator（扩 crate）**
- 创建 `src/events.rs`：`OrchestratorRunEventEmitter`（run.statusChanged/task.statusChanged/task.assigned/run.planUpdated/task.output）。
- 创建 `src/plan.rs`：`trait PlanProducer` + `LlmPlanProducer`（一次性结构化规划调用）。
- 创建 `src/worker.rs`：`trait WorkerRunner` + `ConversationWorkerRunner`（nomi_agent_run 配方）+ `WorkerOutcome`。
- 创建 `src/run_service.rs`：`RunService`（create/get/list/lifecycle/plan）。
- 创建 `src/engine.rs`：`RunEngine`（调度器：注册表+start/stop/run_loop/start_sweeper/resume）。
- 修改 `src/state.rs`：`OrchestratorRouterState` 加 `run_service` + `engine`。
- 修改 `src/routes.rs`：加 `/api/orchestrator/runs` CRUD + cancel。
- 修改 `src/lib.rs`：re-export 新类型。
- 创建 `src/caps.rs`（或在 gateway crate，见 Task 7）。

**后端 — nomifun-gateway**
- 创建 `src/caps_orchestrator.rs`（nomi_run_create/status/result）+ 修改 `src/lib.rs`（mod+build）+ `src/deps.rs`（orchestrator_run_service 字段）。

**后端 — nomifun-app**
- 修改 `src/router/state.rs`：`build_orchestrator_run_state`（扩 build_orchestrator_state）+ ModuleStates 装配。
- 修改 `src/router/routes.rs`：GatewayDeps 装配 orchestrator_run_service。
- 创建 `tests/orchestrator_run_e2e.rs`：HTTP 创建 run → 执行 → 完成。

---

## Task 1: Run/Task/Dep/Assignment 仓库

**Files:** Create `crates/backend/nomifun-db/src/repository/orch_run.rs` + `sqlite_orch_run.rs`；Modify `repository/mod.rs`、`src/lib.rs`；Test 内联。

**Interfaces produced:**
```rust
#[async_trait::async_trait]
pub trait IRunRepository: Send + Sync {
    // runs
    async fn create_run(&self, p: CreateRunParams) -> Result<OrchRunRow, sqlx::Error>;
    async fn get_run(&self, id: &str) -> Result<Option<OrchRunRow>, sqlx::Error>;
    async fn list_runs(&self, workspace_id: &str) -> Result<Vec<OrchRunRow>, sqlx::Error>;
    async fn list_runs_by_status(&self, status: &str) -> Result<Vec<OrchRunRow>, sqlx::Error>; // boot-resume
    async fn update_run(&self, id: &str, p: UpdateRunParams) -> Result<(), sqlx::Error>;       // status/summary/lead_conv_id/total_tokens
    // tasks
    async fn create_task(&self, p: CreateTaskParams) -> Result<OrchRunTaskRow, sqlx::Error>;
    async fn list_tasks(&self, run_id: &str) -> Result<Vec<OrchRunTaskRow>, sqlx::Error>;
    async fn get_task(&self, id: &str) -> Result<Option<OrchRunTaskRow>, sqlx::Error>;
    async fn update_task(&self, id: &str, p: UpdateTaskParams) -> Result<(), sqlx::Error>;      // status/conversation_id/output_summary/output_files/attempt/tokens/graph_x/graph_y
    // deps
    async fn add_dep(&self, blocker: &str, blocked: &str) -> Result<(), sqlx::Error>;
    async fn list_deps(&self, run_id: &str) -> Result<Vec<OrchRunTaskDepRow>, sqlx::Error>;
    /// ready = task.status=='pending' AND every blocker task is 'done'. Returns ready task rows.
    async fn list_ready_tasks(&self, run_id: &str) -> Result<Vec<OrchRunTaskRow>, sqlx::Error>;
    // assignments
    async fn create_assignment(&self, p: CreateAssignmentParams) -> Result<OrchAssignmentRow, sqlx::Error>;
    async fn list_assignments(&self, run_id: &str) -> Result<Vec<OrchAssignmentRow>, sqlx::Error>;
    async fn get_assignment_for_task(&self, task_id: &str) -> Result<Option<OrchAssignmentRow>, sqlx::Error>;
}
// Param structs (id minted internally via generate_prefixed_id run_/rtask_/asg_):
pub struct CreateRunParams { pub workspace_id: String, pub user_id: String, pub goal: String, pub fleet_snapshot: String, pub autonomy: String, pub max_parallel: Option<i64> }
pub struct UpdateRunParams { pub status: Option<String>, pub summary: Option<Option<String>>, pub lead_conv_id: Option<Option<i64>>, pub total_tokens: Option<Option<i64>> }
pub struct CreateTaskParams { pub run_id: String, pub title: String, pub spec: String, pub task_profile: Option<String>, pub status: String, pub graph_x: Option<f64>, pub graph_y: Option<f64> }
pub struct UpdateTaskParams { pub status: Option<String>, pub conversation_id: Option<Option<i64>>, pub output_summary: Option<Option<String>>, pub output_files: Option<Option<String>>, pub attempt: Option<i64>, pub tokens: Option<Option<i64>>, pub graph_x: Option<f64>, pub graph_y: Option<f64> }
pub struct CreateAssignmentParams { pub task_id: String, pub member_id: String, pub score: Option<f64>, pub rationale: Option<String>, pub source: String, pub locked: bool }
```
- Consumes: `OrchRunRow`/`OrchRunTaskRow`/`OrchRunTaskDepRow`/`OrchAssignmentRow`（P0 已建，`nomifun_db::*`）、`generate_prefixed_id`、now-ms helper（同 P0 Task 2/3）。
- `list_ready_tasks` SQL: tasks WHERE run_id=? AND status='pending' AND NOT EXISTS (SELECT 1 FROM orch_run_task_deps d JOIN orch_run_tasks bt ON bt.id=d.blocker_task_id WHERE d.blocked_task_id=tasks.id AND bt.status!='done').

参照模板：`repository/sqlite_orch_fleet.rs`（P0，事务+多实体同文件）、`repository/webhook.rs`。

- [ ] **Step 1: 写仓库测试（失败优先）** — `sqlite_orch_run.rs` 内：create run→create 3 tasks (A,B,C)→add_dep(A→B),(B→C)→`list_ready_tasks` 返 [A]（B,C 被阻塞）→update_task(A,done)→ready 返 [B]→update_task(B,done)→ready 返 [C]；create_assignment + get_assignment_for_task 往返；list_runs_by_status('running')。断言 id 前缀 run_/rtask_/asg_。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-db orch_run`。
- [ ] **Step 3: 写 trait + sqlite impl + mod.rs/lib.rs re-export**（仿 P0 fleet 仓库；list_ready_tasks 用上面 SQL）。
- [ ] **Step 4: 跑确认通过** + `cargo build -p nomifun-db`。
- [ ] **Step 5: 提交** — `git commit -m "feat(orchestrator): Run/Task/Dep/Assignment 仓库"`

---

## Task 2: api-types DTO（Run/Task/Plan）

**Files:** Modify `crates/backend/nomifun-api-types/src/orchestrator.rs` + `lib.rs`；Test 内联。

**Interfaces produced:**
```rust
pub struct Run { pub id: String, pub workspace_id: String, pub goal: String, pub autonomy: String, pub max_parallel: Option<i64>, pub status: String, pub summary: Option<String>, pub lead_conv_id: Option<i64>, pub total_tokens: Option<i64>, pub created_at: i64, pub updated_at: i64 }
pub struct RunTask { pub id: String, pub run_id: String, pub title: String, pub spec: String, pub task_profile: Option<TaskProfile>, pub status: String, pub conversation_id: Option<i64>, pub output_summary: Option<String>, pub output_files: Vec<String>, pub attempt: i64, pub tokens: Option<i64>, pub graph_x: Option<f64>, pub graph_y: Option<f64> }
pub struct RunTaskDep { pub blocker_task_id: String, pub blocked_task_id: String }
pub struct Assignment { pub id: String, pub task_id: String, pub member_id: String, pub score: Option<f64>, pub rationale: Option<String>, pub source: String, pub locked: bool }
pub struct TaskProfile { pub kind: String, pub needs_vision: bool, pub needs_long_context: bool, pub needs_high_reasoning: bool, pub bulk: bool }
pub struct RunDetail { pub run: Run, pub tasks: Vec<RunTask>, pub deps: Vec<RunTaskDep>, pub assignments: Vec<Assignment> }
pub struct CreateRunRequest { pub workspace_id: String, pub goal: String, pub fleet_id: String, pub autonomy: Option<String>, pub max_parallel: Option<i64> }
// 规划产物(PlanProducer 输出 + nomi_run_plan 入参):
pub struct PlannedTask { pub title: String, pub spec: String, pub task_profile: Option<TaskProfile>, pub depends_on: Vec<usize>, pub member_index: Option<usize>, pub rationale: Option<String> }
pub struct PlannedDag { pub tasks: Vec<PlannedTask> }
```
- `output_files` 在 DTO 是 `Vec<String>`（Row 存 JSON String，service 解码）；`task_profile` 同（Row JSON → 结构）。`depends_on`/`member_index` 用任务/成员的 0-based 序号（规划阶段还没 id）。

参照模板：P0 `orchestrator.rs` DTO（已有 Fleet/FleetMember）；double_option（patch 字段）。

- [ ] **Step 1: 写 serde round-trip 测试（失败优先）** — `PlannedDag`（2 task，depends_on=[0]）+ `CreateRunRequest` + `RunDetail` 序列化/反序列化字段一致。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-api-types orchestrator`。
- [ ] **Step 3: 写 DTO + lib.rs re-export**。
- [ ] **Step 4: 跑确认通过**。
- [ ] **Step 5: 提交** — `git commit -m "feat(orchestrator): Run/Plan DTO"`

---

## Task 3: OrchestratorRunEventEmitter + 前端事件类型镜像

**Files:** Create `crates/backend/nomifun-orchestrator/src/events.rs`；Modify `src/lib.rs`；Create `ui/src/common/types/orchestrator/orchestratorEvents.ts`（手写镜像）；Test 内联。

**Interfaces produced:**
```rust
#[derive(Clone)]
pub struct OrchestratorRunEventEmitter { bus: Arc<dyn EventBroadcaster> }
impl OrchestratorRunEventEmitter {
    pub fn new(bus: Arc<dyn EventBroadcaster>) -> Self;
    pub fn emit_run_status(&self, run_id: &str, status: &str);                 // "orchestrator.run.statusChanged"
    pub fn emit_run_plan_updated(&self, run_id: &str);                          // "orchestrator.run.planUpdated"
    pub fn emit_task_status(&self, run_id: &str, task_id: &str, status: &str);  // "orchestrator.task.statusChanged"
    pub fn emit_task_assigned(&self, run_id: &str, task_id: &str, member_id: &str); // "orchestrator.task.assigned"
    pub fn emit_run_completed(&self, run_id: &str, status: &str);               // "orchestrator.run.completed"
}
```
- Consumes: `nomifun_realtime::EventBroadcaster`、`WebSocketMessage::new(name, serde_json::Value)`。
- 前端镜像（手写，team 先例 lighter-weight，非 ts-rs）：`orchestratorEvents.ts` 定义各事件 payload 类型 `{ run_id, task_id?, status?, member_id? }`。

参照模板：`crates/backend/nomifun-cron/src/events.rs`（CronEventEmitter 结构）；`ui/src/common/types/team/teamTypes.ts`（手写事件类型）。

- [ ] **Step 1: 写 emit 测试（失败优先）** — 用一个 mock `EventBroadcaster`（捕获 broadcast 的 WebSocketMessage），断言 `emit_task_status` 产出 name=="orchestrator.task.statusChanged" 且 data 含 run_id/task_id/status。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-orchestrator events`。
- [ ] **Step 3: 实现 events.rs + lib.rs re-export + 前端 orchestratorEvents.ts**。
- [ ] **Step 4: 跑确认通过** + `cd ui && npm run typecheck`。
- [ ] **Step 5: 提交** — `git commit -m "feat(orchestrator): Run 事件发射器 + 前端类型"`

---

## Task 4: PlanProducer（主管规划）

**Files:** Create `crates/backend/nomifun-orchestrator/src/plan.rs`；Modify `src/lib.rs`；Test 内联（mock）。

**Interfaces produced:**
```rust
#[async_trait::async_trait]
pub trait PlanProducer: Send + Sync {
    /// 把目标拆成任务 DAG。members 是 fleet 成员快照(供按 index 分派)。
    async fn produce(&self, goal: &str, members: &[FleetMember]) -> Result<PlannedDag, AppError>;
}
/// 真实实现:一次性结构化 LLM 调用(用一个"lead"模型)产出 PlannedDag JSON。
pub struct LlmPlanProducer { provider_repo: Arc<dyn IProviderRepository>, lead: ProviderWithModel }
impl LlmPlanProducer { pub fn new(provider_repo: Arc<dyn IProviderRepository>, lead: ProviderWithModel) -> Self; }
#[async_trait::async_trait] impl PlanProducer for LlmPlanProducer { /* one_shot_completion → parse JSON → PlannedDag; 解析失败 fail-soft 回退单任务(整个 goal 作一个 task) */ }
```
- Consumes: `FleetMember`/`PlannedDag` DTO；`one_shot_completion`（`nomifun-ai-agent` factory/provider_config.rs，P0 agent-model 探查确认存在；签名以该文件为准）；`ProviderWithModel`。
- 规划提示词：要求模型输出 JSON `{tasks:[{title,spec,task_profile?,depends_on:[idx],member_index?,rationale?}]}`；强约束「depends_on 用任务序号、member_index 用成员序号、无环」。**fail-soft**：解析失败 → 回退为单任务 DAG（title=goal 截断、spec=goal、无依赖、member_index=0），记 warn——保证引擎永远有可执行计划。

参照模板：`one_shot_completion` 调用点（IDMM sidecar 用过）；team prompts 风格。

- [ ] **Step 1: 写 mock + fail-soft 测试（失败优先）** — `MockPlanProducer` 返固定 2-task DAG（测 trait 形态）；`LlmPlanProducer` 的 JSON 解析单元（喂合法 JSON→PlannedDag；喂垃圾→回退单任务）——把解析抽成可单测的 `parse_plan(&str) -> PlannedDag`（fail-soft）。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-orchestrator plan`。
- [ ] **Step 3: 实现 trait + parse_plan(fail-soft) + LlmPlanProducer**（一次性调用接 one_shot_completion；若签名不符就近适配）。
- [ ] **Step 4: 跑确认通过**。
- [ ] **Step 5: 提交** — `git commit -m "feat(orchestrator): PlanProducer 主管规划(LLM+fail-soft)"`

---

## Task 5: WorkerRunner（worker=会话执行）

**Files:** Create `crates/backend/nomifun-orchestrator/src/worker.rs`；Modify `src/lib.rs`；Test 内联（mock + 真实路径对 Mock AgentInstance）。

**Interfaces produced:**
```rust
pub struct WorkerOutcome { pub conversation_id: i64, pub text: Option<String>, pub ok: bool }
#[async_trait::async_trait]
pub trait WorkerRunner: Send + Sync {
    /// 在一条新 worker 会话上执行一个任务,阻塞至完成或超时,返回最终文本。
    async fn run(&self, member: &FleetMember, workspace_dir: Option<&str>, run_id: &str, task_id: &str, brief: &str, task_spec: &str, timeout: Duration) -> Result<WorkerOutcome, AppError>;
}
pub struct ConversationWorkerRunner { conv: ConversationService, task_manager: Arc<dyn IWorkerTaskManager>, user_id: String }
impl ConversationWorkerRunner { pub fn new(conv: ConversationService, task_manager: Arc<dyn IWorkerTaskManager>, user_id: String) -> Self; }
```
- 真实实现 = **复刻 `nomi_agent_run` 配方**（`caps_conversation.rs:426-503` 为权威模板）：
  1. `ProviderWithModel { provider_id: member.provider_id, model: member.model.clone(), use_model: member.model.clone() }`（ACP 引擎可空模型——P1 先支持 Nomi 引擎成员；非 Nomi 成员超出 P1 范围，记 warn 跳过/失败）。
  2. `extra = json!({ "session_mode":"yolo", "desktopGateway":true, "orchestrator_run_id":run_id, "orchestrator_task_id":task_id, "system_prompt": brief })`；有 workspace_dir 则 `extra["workspace"]=...`。
  3. `conv.create(&user_id, CreateConversationRequest{ r#type: AgentType::Nomi, name: Some(format!("Run {run_id} · {task_id}")), model: Some(pwm), source:None, channel_chat_id:None, extra })`。
  4. `conv.send_message(&user_id, &conv.id.to_string(), SendMessageRequest{content: task_spec, files:vec![], inject_skills:vec![], hidden:false, origin:Some("orchestrator".into()), channel_platform:None}, &task_manager)`。
  5. `await_turn`（poll `conv.runtime_summary_for(id).is_processing` 每 500ms 至 timeout）→ settle（再 await 5s 细 poll 25ms，避 reasoning model text:null）→ `read_final_text`（list_messages desc 取 position==left&type==text）。
- Consumes: `ConversationService`（create/send_message/runtime_summary_for/list_messages）、`IWorkerTaskManager`、`CreateConversationRequest`/`SendMessageRequest`/`ListMessagesQuery`、`AgentType`、`ProviderWithModel`。**把 await_turn/read_final_text 私有 helper 照搬进 worker.rs**（caps_conversation.rs:303-363）。

参照模板：`crates/backend/nomifun-gateway/src/caps_conversation.rs`（agent_run/await_turn/read_final_text/latest_assistant_text 全套）。

- [ ] **Step 1: 写测试（失败优先）** — `MockWorkerRunner`（返固定 WorkerOutcome，供 Task 6 引擎测试用，定义在此）；`ConversationWorkerRunner` 对 **Mock AgentInstance**（`AgentInstance::Mock`，via 一个测试 task_manager + in-memory ConversationService over init_database_memory）跑一个 task → 得到 mock agent 的回显文本 + conversation_id。若搭 Mock 链路过重，至少单测 `read_final_text`/`latest_assistant_text` 解析 + extra 组装正确，并在报告说明真实链路由 Task 9 集成测试覆盖。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-orchestrator worker`。
- [ ] **Step 3: 实现 trait + helper 照搬 + ConversationWorkerRunner**。
- [ ] **Step 4: 跑确认通过** + `cargo build -p nomifun-orchestrator`。
- [ ] **Step 5: 提交** — `git commit -m "feat(orchestrator): WorkerRunner(worker=真实会话)"`

---

## Task 6: RunService + RunEngine（调度器，串行）

**Files:** Create `crates/backend/nomifun-orchestrator/src/run_service.rs` + `src/engine.rs`；Modify `src/lib.rs`、`src/state.rs`（OrchestratorRouterState 加 run_service+engine）；Test 内联（mock PlanProducer + mock WorkerRunner）。

**Interfaces produced:**
```rust
#[derive(Clone)]
pub struct RunService { run_repo: Arc<dyn IRunRepository>, fleet_repo: Arc<dyn IFleetRepository>, ws_repo: Arc<dyn IOrchWorkspaceRepository>, planner: Arc<dyn PlanProducer>, emitter: OrchestratorRunEventEmitter }
impl RunService {
    pub fn new(...) -> Self;
    pub async fn create(&self, user_id:&str, req: CreateRunRequest) -> Result<Run, AppError>;   // snapshot fleet, status=planning
    pub async fn get_detail(&self, id:&str) -> Result<RunDetail, AppError>;
    pub async fn list(&self, workspace_id:&str) -> Result<Vec<Run>, AppError>;
    pub async fn plan(&self, run_id:&str) -> Result<(), AppError>;  // planner.produce → persist tasks+deps+assignments(member_index→member_id) → emit planUpdated → status=running
    pub async fn cancel(&self, run_id:&str) -> Result<(), AppError>;
}
#[derive(Clone)]
pub struct RunEngine { /* deps: run_repo, fleet_repo, worker: Arc<dyn WorkerRunner>, emitter, handles: Arc<DashMap<String, RunHandle>>, generation: Arc<AtomicU64> */ }
impl RunEngine {
    pub fn new(deps: RunEngineDeps) -> Self;
    pub fn start(&self, run_id: String);            // spawn run_loop; idempotent (stop then start)
    pub fn stop(&self, run_id: &str);
    pub fn is_running(&self, run_id: &str) -> bool;
    pub fn resume_persisted_runs(&self, run_repo: Arc<dyn IRunRepository>); // boot: list_runs_by_status('running') → start each
}
```
- **RunEngine::run_loop(run_id)**（串行）：loop { cancel-check；`list_ready_tasks(run_id)`；若空且无 in-flight → 检查是否全 done → 是则 run.status=completed + 聚合 summary（P1 聚合=拼接各 task.output_summary，或一次 planner-style 调用；先用拼接，记 TODO 可升级）+ emit_run_completed + break；否则（有未就绪但被阻塞/失败）按状态收尾 break。取一个 ready task → assignment 取 member → emit_task_status(running) + update_task(status=running) → `worker.run(member, ws, run_id, task_id, brief, spec, timeout)` → 成功:update_task(status=done, conversation_id, output_summary=text) + emit + check_unblocks；失败:update_task(status=failed) + emit（P1 失败即标记，重试/改派是 P3）→ continue }。
- **brief 组装**：worker 的 system_prompt = 角色提示(member.role_hint) + 任务标题/规格 + 上游已完成任务的 output_summary（注入上下文）。
- `plan()` 把 `PlannedTask.depends_on`(序号)→ 实际 task id 边；`member_index`→ fleet_snapshot 成员 → assignment(source='auto', rationale)。
- Consumes: Task 1 repo、Task 4 PlanProducer、Task 5 WorkerRunner、Task 3 emitter；`DashMap`、`AtomicU64`、`tokio::spawn`。
- 引擎注册表/start/stop/generation/HandleGuard/resume 复刻 `nomifun-requirement::Orchestrator`（orchestrator.rs:142-363）。**P1 串行**：run_loop 一次跑一个 ready task（不 spawn 多 worker）。

参照模板：`crates/backend/nomifun-requirement/src/orchestrator.rs`（Orchestrator/start/run_loop/resume）；`team scheduler` check_unblocks。

- [ ] **Step 1: 写引擎集成单测（失败优先，全 mock）** — 用 `SqliteRunRepository` over `init_database_memory` + `MockPlanProducer`(返 A→B→C 链 DAG) + `MockWorkerRunner`(每任务返固定文本)。`RunService.create`→`plan`→ 断言 3 tasks+2 deps+3 assignments 落库、run.status=running。然后 `RunEngine.start(run_id)`，轮询至 run.status=completed（有界 await），断言任务按依赖序全 done、output_summary 落库、summary 非空。再测 cancel 中途停。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-orchestrator engine`。
- [ ] **Step 3: 实现 run_service.rs + engine.rs + state.rs 扩展**。
- [ ] **Step 4: 跑确认通过** + `cargo build -p nomifun-orchestrator`。
- [ ] **Step 5: 提交** — `git commit -m "feat(orchestrator): RunService + RunEngine 串行调度器"`

---

## Task 7: caps_orchestrator gateway 域

**Files:** Create `crates/backend/nomifun-gateway/src/caps_orchestrator.rs`；Modify `src/lib.rs`（mod+build）、`src/deps.rs`（字段）；Test（注册不变量在既有 registry 自测中自动覆盖；本任务加 1 个 caps 计数/命名断言若有既有测试文件）。

**Interfaces produced:** 工具（读为主，写经 RunService/RunEngine）：
- `nomi_run_create`（Write）：入 {workspace_id, goal, fleet_id, autonomy?} → RunService.create + plan + engine.start → 返 run_id。
- `nomi_run_status`（Read）：入 {run_id} → RunDetail 精简（run.status + 各 task status）。
- `nomi_run_result`（Read）：入 {run_id} → run.summary + 各 task output_summary（未完返 status=running）。
- handler 形如 `async fn(deps: Arc<GatewayDeps>, p: P) -> Value`，经 `deps.orchestrator_run_service` / `deps.orchestrator_run_engine`（或合并为一个 facade Arc）。
- GatewayDeps 加 `pub orchestrator_run_service: Arc<nomifun_orchestrator::RunService>` + `pub orchestrator_run_engine: Arc<nomifun_orchestrator::RunEngine>`（或一个 `OrchestratorRunFacade`）。

参照模板：`crates/backend/nomifun-gateway/src/caps_agent.rs`/`caps_companion.rs`（register fn + Capability::new::<P,_,_> + CapabilityMeta::new）；lib.rs build()。

- [ ] **Step 1: 写注册断言测试（失败优先）** — 若 gateway 有 registry 自测(如 `registry_builds_and_names_fit_mcp_limit`)，确认新工具被收录且名 ≤42；否则加一个最小测试断言 `Registry::global().tool_visible(Surface::Desktop, "nomi_run_status")`。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-gateway orchestrator`（或相应测试名）。
- [ ] **Step 3: 实现 caps_orchestrator.rs + lib.rs mod/build + deps.rs 字段**。注意 caps 计数 floor 自测可能需更新阈值。
- [ ] **Step 4: 跑确认通过** + `cargo build -p nomifun-gateway`。
- [ ] **Step 5: 提交** — `git commit -m "feat(orchestrator): caps_orchestrator gateway 工具"`

---

## Task 8: Run REST 路由

**Files:** Modify `crates/backend/nomifun-orchestrator/src/routes.rs` + `src/state.rs`（OrchestratorRouterState 已在 Task 6 加 run_service+engine）；Test 内联 router-builds。

**Interfaces produced:**
- `POST /api/orchestrator/runs`（CreateRunRequest → create+plan+engine.start → 201 Run）
- `GET /api/orchestrator/workspaces/{ws}/runs`（list）；`GET /api/orchestrator/runs/{id}`（RunDetail）
- `POST /api/orchestrator/runs/{id}/cancel`（200）
- handler 薄,Extension<CurrentUser>,ApiResponse 包装（同 P0 routes.rs）。

参照模板：P0 `nomifun-orchestrator/src/routes.rs`（fleet/workspace 路由形态）。

- [ ] **Step 1: 写 router-builds 测试（失败优先）** — 构造 OrchestratorRouterState（含 run_service over in-memory + mock planner + engine w/ mock worker），`orchestrator_routes(state)` 不 panic。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-orchestrator routes`。
- [ ] **Step 3: 实现 run 路由**。
- [ ] **Step 4: 跑确认通过**。
- [ ] **Step 5: 提交** — `git commit -m "feat(orchestrator): Run REST 路由"`

---

## Task 9: app 接线 + 端到端集成测试

**Files:** Modify `crates/backend/nomifun-app/src/router/state.rs`（`build_orchestrator_run_state` 扩 `build_orchestrator_state`）、`src/router/routes.rs`（GatewayDeps）；Create `crates/backend/nomifun-app/tests/orchestrator_run_e2e.rs`。

**接线（照 service-wiring 探查 notes）：**
- `build_orchestrator_state(services)` 扩为构造：fleet/ws/run 仓库 + 独立 ConversationService（`ConversationService::new(services.work_dir, services.event_bus, ExtensionSkillResolver::new(services.skill_paths), services.worker_task_manager, conv_repo, agent_metadata_repo, acp_session_repo).with_runtime_state(services.conversation_runtime_state).with_failover_deps(...)`）+ `LlmPlanProducer`（lead 模型：P1 取 fleet 首成员的 provider+model，或一个合理默认；无则规划回退单任务）+ `ConversationWorkerRunner` + `OrchestratorRunEventEmitter::new(services.event_bus)` → `RunService` + `RunEngine`。**build 时** `engine.resume_persisted_runs(run_repo)`（boot-resume running runs，tokio::spawn 分离）。返回 `OrchestratorRouterState{ fleet, workspace, run_service, engine }`。
- ModuleStates 装配点不变（已 `orchestrator: build_orchestrator_state(services)`）。
- GatewayDeps 装配：加 `orchestrator_run_service: states.orchestrator.run_service.clone()`（+ engine 若分开）。

**集成测试（mock 不可用则用真 Mock provider/agent）：** 起测试 app（带认证），`POST /api/orchestrator/runs`（先建 workspace+fleet via P0 端点；fleet 成员用一个测试 provider+model）→ 轮询 `GET /runs/{id}` 至 status=completed（有界）→ 断言 tasks 全 done、run.summary 非空。**若真 LLM 不可得**：本集成测试用一个注入的 mock planner+worker 的测试专用 router 构造（参照现有 e2e 如何注入测试依赖），或断言到 status=running + 计划已落库（tasks>0），并在报告说明真实 LLM 路径由真机验收覆盖。**关键闸：`cargo build -p nomifun-app` 必过。**

参照模板：`build_requirement_state`（state.rs:769-864，权威）；P0 `orchestrator_e2e.rs`；现有 e2e 测试注入测试依赖的方式。

- [ ] **Step 1: 写集成测试（失败优先）** — 见上;先 RED(run 路由/引擎未接 → 404 或 run 不前进)。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-app orchestrator_run`。
- [ ] **Step 3: 实现 build_orchestrator_run_state 扩展 + GatewayDeps 装配**。
- [ ] **Step 4: `cargo build -p nomifun-app`（关键闸）+ 跑集成测试确认通过**。
- [ ] **Step 5: 提交** — `git commit -m "feat(orchestrator): Run 引擎 app 接线 + 端到端集成测试"`

---

## Self-Review（对照 spec P1 切片）

**Spec 覆盖：** §3 Run/Task 状态机 → Task 1/6；§4.2 worker=会话 → Task 5；§4.3 产物回流 → Task 6（brief 注入上游 output）；§4.4 主管规划+确定性调度 → Task 4(规划)+Task 6(调度,串行)；§6 分派 → Task 6 plan()（member_index→assignment；能力打分是 P3,P1 用 planner 的 member_index 直选）；§8 gateway → Task 7；§5 持久化 → Task 1（无新迁移,复用 018）。**P1 不含**:真并行(P2)、能力 Router 打分(P3)、画布(P1b)、自主三级完整(P1 默认 yolo worker,守护/协同 P3)、team 移除(P5)。

**占位符扫描：** 无 TBD;聚合 summary 用「拼接」是 P1 明确简化(注 TODO 可升级 lead 调用);失败即标记(重试 P3)是 P1 范围边界。代码步给出真实签名/SQL/配方;样板指向模板路径。

**类型一致：** `IRunRepository.list_ready_tasks`(Task1)→RunEngine(Task6)消费;`PlanProducer.produce`/`WorkerRunner.run`签名(Task4/5)→RunEngine(Task6)消费;`OrchestratorRouterState{fleet,workspace,run_service,engine}`(Task6)→routes(Task8)→app 装配(Task9)一致;DTO 字段(Task2)↔Row(P0 Task1)↔前端事件(Task3)对齐。

## Execution Handoff

依赖波次：Task 1(repo)→Task 2(DTO,可与1并行)→Task 3(events)/Task 4(planner)/Task 5(worker) 三者依赖 1+2 可并行 →Task 6(引擎,依赖 3/4/5)→Task 7(caps)/Task 8(routes) 依赖 6 →Task 9(app 接线,依赖 7+8)。
执行：**subagent-driven-development**，每任务 implementer→两阶评审→fix loop→记账;引擎核心(Task 5/6)用 opus,机械任务 sonnet。

# Phase 3a — 动态派发底座（单持久 run + 运行时追加 + 中途批次观测）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让一个会话用**一个持续生长的 run**：master 可随时往同一个 run 追加节点（运行时 DAG 增长，无需暂停/无审批/无规划器），并在一批节点完成后收到**中途观测回执**——从而形成 Claude Code 式「派发→观测→再派发」循环。采用分叉默认 F3（已有 run 就追加）+ F4（按批节流观测）+ F5（动态派发默认标配）。

**Architecture:** 零重造引擎。运行时追加复用既有事务原语 `reconcile_run_plan`（清依赖+删未保留+插新+重建依赖），以**空删除列表**表达「纯追加」；`RunEngine::add_tasks` 在 `RunLocks` 锁内插入+重臂，与终态判定原子互斥；`run_loop` 零改（`list_ready_tasks` 每轮重查自取新节点）。单 run 收敛=网关在 create/spawn 前读 `extra.orchestrator_run_id`，有可追加的 run 就走 `add_tasks` 并**跳过 create_adhoc + link**（不重绑、不产孤儿）。中途观测=`run_loop` 分支(d)（inflight 非空、与终态守卫结构上不相交）按时间/数量节流发 `RunOutcome::BatchProgress` 回执，复用 Phase 1a 的 `LeadReporter` 缝。

**Tech Stack:** Rust（`nomifun-orchestrator` run_service/engine、`nomifun-gateway` caps_orchestrator、`nomifun-app` state.rs），cargo-nextest。无 FE、无迁移。

## Global Constraints

- **测试**：`cargo nextest run -p nomifun-orchestrator -p nomifun-gateway`；改 `RunOutcome` 后**必 `cargo check -p nomifun-app`**（`compose_lead_receipt` 穷尽 match 跨 crate——Phase 1a 教训：`RunOutcome` 加变体致 nomifun-app 不编译）。无 `| tail`。
- **不变量（载重）**：`RunEngine::add_tasks` **必须**持 `RunLocks::for_run` 锁包住「插入 + 重臂」，与 `run_loop` 终态判定原子互斥；重臂的 `matches!` 状态判定**必在锁内 fresh get_run**（非方法顶快照），否则 append-vs-`finish_run` 竞态会造出「终态 run 带未跑 pending 节点且无 driver」（boot-resume 只 re-list running → 永不恢复）。
- **不动**：两处 `if current_tasks.iter().any(|t| t.status == "running")` 拒绝（`run_service.rs:1479`/`1534`，属破坏性 `adjust`，保留 pause-first）。`add_tasks` 是**独立追加路径**，不经 compute/apply_adjusted_plan、无此拒绝、无 autonomy gate。
- **中途观测节流**：不得每节点发（前端已有 WS `emit_task_status` 逐节点流）；按 `BATCH_REPORT_MIN_NODES`(数量) AND `BATCH_REPORT_INTERVAL`(时间) 节流；`lead_conv_id` 为 None 短路；digest 用 `build_summary_digest`（有界）。绝不在 `RunLocks` 终态守卫内 await（分支(d) inflight 非空已结构不相交）。
- **Git**：分支 `feat/phase1-reliability-shared-context`（续，叠栈）。**Task 2 ∥ Task 3 用 worktree 隔离并行**（见执行说明），其余串行。提交前 `git pull --rebase`。

## 任务依赖 / 并行

```
Task 1 (add_tasks 原语; run_service.rs + engine.rs) ── 基础，串行
   ├──> Task 2 (网关单run收敛+工具; caps_orchestrator.rs) ──┐ 二者文件不相交
   └──> Task 3 (中途批次观测; engine.rs + nomifun-app)     ──┘ → worktree 并行后合并
```
- Task 2 依赖 Task 1（用 `engine.add_tasks`）；Task 3 独立于 Task 1/2（只加 run_loop 钩子 + RunOutcome 变体 + app arm）。
- **但 Task 1 与 Task 3 都动 engine.rs（不同函数）** → Task 3 须在 Task 1 合入后开；Task 2 与 Task 3 文件不相交（gateway vs engine+app）→ **可并行 worktree**。

---

### Task 1: `add_tasks` 运行时追加原语（run_service + engine + RunLocks）

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/run_service.rs`（新增 `add_tasks`；置于 `plan_flat`(498)/`apply_adjusted_plan`(1516) 旁）
- Modify: `crates/backend/nomifun-orchestrator/src/engine.rs`（新增 `RunEngine::add_tasks` 包装，仿 `adjust` 锁块 866-871 / `rerun_task` 762）
- Test: `engine.rs` mod tests

**Interfaces:**
- Produces:
  - `RunService::add_tasks(&self, run_id: &str, tasks: Vec<PlannedTask>) -> Result<Run, AppError>`
  - `RunEngine::add_tasks(&self, run_service: &RunService, run_id: &str, tasks: Vec<PlannedTask>) -> Result<Run, AppError>`（持 `RunLocks::for_run`）
- Consumes（复用，勿新造）：`decode_fleet_snapshot`、`resolve_assignment_pick`（纯 pick）、`reconcile_run_plan`(`orch_run.rs:309`) + `ReconcilePlan`/`ReconcileNewTask`/`ReconcileDepRef`、`CreateTaskParams`(`orch_run.rs:52`)、`emit_run_plan_updated`、apply_adjusted_plan 的重臂尾(`run_service.rs:1729-1757`)。
- 语义：`delete_task_ids: vec![]`（纯追加，不删/不改 running 节点）；深度依赖 `depends_on: Vec<usize>`（batch 内索引→`NewIndex`；对既有节点用 `Kept(existing_id)`）；重臂只对 `completed|failed|cancelled|completed_with_failures` 翻回 running（**排除 paused/awaiting_plan_approval**）；无 autonomy gate。

- [ ] **Step 1: 写失败测试**（`engine.rs` mod tests）
```rust
    #[tokio::test]
    async fn add_tasks_appends_to_running_run_and_new_node_runs() {
        // 一个进行中/或刚完成的 run,add_tasks 追加一个 pending 节点 → 它被派发跑完,
        // run 最终 completed(含新节点 done)。
        let worker = Arc::new(MockWorkerRunner::with_text(1, "x"));
        let (svc, engine, run_id) = single_task_harness(worker.clone()).await; // Phase1a 已有
        engine.start(run_id.clone());
        let _ = drive_to_completion(&svc, &run_id).await; // 原节点跑完 → run 到 completed
        // 追加一个新节点(无依赖) 到终态 run
        let new = vec![planned_task("追加节点", "do the extra work")];
        engine.add_tasks(&svc, &run_id, new).await.expect("add_tasks");
        // 重臂后 run 回 running;若 loop 已死则 engine.start 复活(测试里显式)
        if !engine.is_running(&run_id) { engine.start(run_id.clone()); }
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(detail.run.status, "completed");
        assert!(detail.tasks.iter().any(|t| t.title.contains("追加节点") && t.status == "done"),
            "appended node must run to done");
    }

    #[tokio::test]
    async fn add_tasks_empty_is_rejected() {
        let (svc, engine, run_id) = single_task_harness(Arc::new(MockWorkerRunner::with_text(1,"x"))).await;
        assert!(engine.add_tasks(&svc, &run_id, vec![]).await.is_err());
    }
```
> `planned_task(title, spec)`：构 `PlannedTask`（`nomifun_api_types`）最小字段（kind 默认 agent、depends_on 空）。若无 helper 则内联构造。`single_task_harness`/`drive_to_completion` Phase1a 已有。

- [ ] **Step 2: 运行确认失败**

Run: `cargo nextest run -p nomifun-orchestrator add_tasks`
Expected: FAIL（`add_tasks` 未定义）。

- [ ] **Step 3: 实现 `RunService::add_tasks`**（`run_service.rs`，按综合 build sheet）
按建 sheet 的 8 步：空检查 → `get_run`(NotFound) → `decode_fleet_snapshot` → 构 `Vec<ReconcileNewTask>`（每个 incoming 都当 new：`CreateTaskParams{status:"pending", on_fail:None, …}` + `resolve_assignment_pick(&members,None,None,None)` 纯 pick + `depends_on` 映射 `NewIndex(i)`/`Kept(existing)`）→ `reconcile_run_plan(run_id, ReconcilePlan{delete_task_ids: vec![], new_tasks})` → `emit_run_plan_updated` → **重臂尾逐字抄 `apply_adjusted_plan` 1729-1757**（fresh `get_run` + `matches!(status,"completed"|"failed"|"cancelled"|"completed_with_failures") → update_run(running)+emit`）→ 返回 `run_row_to_dto`。**无 autonomy gate。**

- [ ] **Step 4: 实现 `RunEngine::add_tasks` 包装（持锁）**（`engine.rs`）
```rust
pub async fn add_tasks(&self, run_service: &crate::run_service::RunService,
    run_id: &str, tasks: Vec<nomifun_api_types::PlannedTask>)
    -> Result<nomifun_api_types::Run, AppError> {
    let lock = self.deps.run_locks.for_run(run_id);   // engine.rs:306
    let _guard = lock.lock().await;
    run_service.add_tasks(run_id, tasks).await
    // 锁在此 drop —— 插入+重臂 与 run_loop 终态判定原子互斥
}
```

- [ ] **Step 5: 跑测试 + 跨 crate**

Run: `cargo nextest run -p nomifun-orchestrator`
Expected: PASS（2 新测试绿 + 既有 adjust/rerun/终态测试不回归）。
Run: `cargo check -p nomifun-gateway -p nomifun-app`
Expected: clean（本任务未改签名/枚举，下游不破——纯新增方法）。

- [ ] **Step 6: 提交**
```bash
git add crates/backend/nomifun-orchestrator/src/run_service.rs crates/backend/nomifun-orchestrator/src/engine.rs
git commit -m "feat(orch): add_tasks 运行时追加原语(reconcile 空删除+RunLocks 原子重臂)"
```

---

### Task 2: 网关单 run 收敛 + `nomi_run_add_tasks` 工具（依赖 Task 1；∥ Task 3）

**Files:**
- Modify: `crates/backend/nomifun-gateway/src/caps_orchestrator.rs`（`read_conversation_run_id` 助手；`create`(194)/`spawn`(775) 收敛分支；`add_tasks` handler；`register`(1044) 加工具；测试固定名 fixups 1399/1430）

**Interfaces:**
- Consumes: Task 1 的 `engine.add_tasks`；`read_conversation_model_range`(414) 作助手模板；`parse_lead_conv_id`；`spawn` 现有 `Vec<PlannedTask>` 组装块（逐字复用作 `add_tasks` 的 `tasks` 实参）。
- Produces: `read_conversation_run_id`（读 `conv.extra["orchestrator_run_id"]`）；`run_is_appendable`（run 非 `cancelled` 且存在→可追加，终态 completed/failed 亦可追加）；工具 `nomi_run_add_tasks`（`AddTasksParams{tasks: Vec<SpawnTask>}`，run_id 由会话链接 run 解析非入参；`.deny_on(ORCHESTRATOR_DENY_SURFACES)`）。
- 收敛：`create`/`spawn` 在 `parse_lead_conv_id` 后、`create_adhoc` 前——若有可追加 run → `engine.add_tasks` + 外层重臂（`if run.status=="running" && !engine.is_running { engine.start }`）+ 返回 run_id，**跳过 create_adhoc 与 link_orchestrator_run**（不重绑）；否则 fall through 现有路径。

- [ ] **Step 1: 写测试**（`caps_orchestrator.rs` mod tests）：`spawn` 到一个已链接 run 的会话 → 走 add_tasks 追加、run_id 不变、不新建 run（断言 orchestrator_run_id 未变 + 任务数增加）。若网关测试难构真 run，则至少测 `read_conversation_run_id` 读取 + 工具注册可见（见 Step 4 fixup）。
- [ ] **Step 2: 运行确认失败。**
- [ ] **Step 3: `read_conversation_run_id` + `run_is_appendable`。**
- [ ] **Step 4: `create`/`spawn` 收敛分支 + `nomi_run_add_tasks` 工具注册 + handler。** 更新两处硬编码工具名测试（`orchestrator_tools_registered_and_visible_on_desktop` 1399、`orchestrator_tools_absent_on_remote_surface` 1430）加新名。
- [ ] **Step 5: 跑测试**：`cargo nextest run -p nomifun-gateway -p nomifun-orchestrator` 全绿；`cargo check -p nomifun-app` clean。
- [ ] **Step 6: 提交** `feat(gateway): 单 run 收敛(已有 run 追加不重绑) + nomi_run_add_tasks 工具`。

---

### Task 3: 中途批次观测回执（∥ Task 2；engine + nomifun-app）

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/engine.rs`（`RunOutcome::BatchProgress` 变体+as_str 120/149；run_loop 分支(d) 钩子 1479-1484；loop 局部 1011）
- Modify: `crates/backend/nomifun-app/src/router/state.rs`（`compose_lead_receipt` 加 `BatchProgress` 臂 999——**必须，否则不编译**）
- Test: `engine.rs` mod tests

**Interfaces:**
- Produces: `RunOutcome::BatchProgress`（`as_str "batch_progress"`）；run_loop 局部 `last_batch_report: Option<Instant>` / `done_since_report: usize`；常量 `BATCH_REPORT_MIN_NODES`（默认 3）+ `BATCH_REPORT_INTERVAL`（默认 20s）。
- Consumes: 既有 `LeadReporter`（Phase1a）；`run.lead_conv_id`（顶层已取，复用勿再 get_run）；`build_summary_digest`(2421)；`emit_task_status`（勿重复）。
- 钩子在分支(d) settle 后（inflight 非空、与终态守卫不相交）：`done_since_report += (完成)`；时间 AND 数量到 → `report(lead_conv_id, run_id, RunOutcome::BatchProgress, &digest)` best-effort(warn on err) → 重置。`lead_conv_id` None 短路。

- [ ] **Step 1: 写测试**：`RecordingLeadReporter` + 一个多节点 run，断言在终态前收到 ≥1 条 `batch_progress` 回执（用小 `BATCH_REPORT_MIN_NODES`/`INTERVAL` 或注入可配值使其触发）。
- [ ] **Step 2: 运行确认失败。**
- [ ] **Step 3: 加 `RunOutcome::BatchProgress` + as_str。**
- [ ] **Step 4: run_loop 分支(d) 钩子 + loop 局部 + 节流常量。**
- [ ] **Step 5: `compose_lead_receipt` 加 BatchProgress 臂**（nomifun-app state.rs:999，见 build sheet 文案：「[编排回执·进展] …刚完成一批节点…可 nomi_run_add_tasks 追加…不新建编排」）。
- [ ] **Step 6: 跑测试 + 跨 crate**：`cargo nextest run -p nomifun-orchestrator` 全绿（既有 lead 测试不回归——批次回执是新增，注意计数型断言）；**`cargo check -p nomifun-app` 必过**（BatchProgress 臂补齐）。
- [ ] **Step 7: 提交** `feat(orch): 中途批次观测回执(RunOutcome::BatchProgress，节流)`。

---

## 执行说明（并行）

- **Task 1 串行先做**（基础，动 run_service+engine），评审合入。
- **Task 2 ∥ Task 3 用 worktree 隔离并行**：二者文件不相交（Task2=gateway caps_orchestrator.rs；Task3=engine.rs 不同函数+nomifun-app state.rs）。各在自己 worktree/分支实现+评审，再依次合回主分支。合并后跑一次复合 `cargo nextest -p nomifun-orchestrator -p nomifun-gateway` + `cargo check -p nomifun-app` 确认交叉无碍。
  > 注：Task 3 也动 engine.rs（Task 1 已动过），故 Task 3 worktree 须从 **Task 1 合入后**的主分支开；与 Task 2（gateway）不相交，可安全并行。

## 自审（Self-Review）

**1. Spec 覆盖（对照 Phase 3 设计 §3 W7a/W7b + 分叉）：** W7a 单 run 追加 → Task1(原语)+Task2(收敛+工具)；F3 已有 run 就追加 → Task2 收敛分支；W7b 中途观测 → Task3；F4 按批节流 → Task3 节流；F5 动态派发标配 → 工具默认注册于所有桌面会话(承接 Phase0)。✅ W7c 自主循环 / W7d 嵌套 → **Phase 3b**（不在本相）。

**2. Placeholder 扫描：** 无 TBD。`add_tasks` 给了 8 步精确复用指令 + 引用既有 `reconcile_run_plan`/重臂尾行号；Task2/3 步骤指向 build sheet 的确切文案与行号。测试 helper 标「Phase1a 已有 / 无则内联」。

**3. 类型一致性：** `add_tasks(run_id, Vec<PlannedTask>)->Run` 在 run_service/engine/测试一致；`RunOutcome::BatchProgress`/`as_str "batch_progress"`/compose_lead_receipt 臂 三处一致；`ReconcilePlan{delete_task_ids: vec![], new_tasks}` 空删除语义一致。

**4. 跨 crate（Phase1a 教训）：** Task3 加 `RunOutcome` 变体 → nomifun-app `compose_lead_receipt` 穷尽 match 必补臂，Step6 强制 `cargo check -p nomifun-app`。Task1/2 无枚举/签名破坏。

**5. 载重不变量：** `RunEngine::add_tasks` 持 `RunLocks` 锁 + 锁内 fresh 重臂 = append-vs-finish_run 原子（Global Constraints + Task1 Step4 强调）；中途观测在分支(d) 不相交于终态守卫（Task3）。

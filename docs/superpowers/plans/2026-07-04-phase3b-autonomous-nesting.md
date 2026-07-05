# Phase 3b — 自主长循环(W7c) + 受控嵌套(W7d) 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让编排 master 能**无人值守自主跑**——带目标自评 plan→dispatch→observe→adapt，直到目标达成/预算耗尽/连续无进展/用户中止；并支持**受控嵌套**（子 agent 可向父 run 追加，深度受限）。这是统一编排的最后一块。

**Architecture（关键裁定）：自主循环是 EMERGENT，不加后端循环、不加新状态、不加迁移。** Phase 3a 已把 run 终态 → `LeadReporter.report` → `steer_message` 无活跃回合时回退 `send_message` = **一个带完整网关工具的全新 lead agent 回合** → 它调 `nomi_run_add_tasks` → `add_tasks` 重臂 run → 又到终态 → 又回报……这个 `finish_run→report→master回合追加→re-arm→run_loop→finish_run` 循环已闭合且 race-safe。W7c 只需：(a) 让回执**感知 autonomy+目标+预算**（自评达成则收尾、未达成且有预算则追加、预算尽则收尾交用户）；(b) **硬预算闸**折进 `run_is_appendable`/两个网关追加口（LLM 绕不过的确定性 backstop）；(c) boot-resume 复用现成。W7d：worker 向父 run 追加**已可工作**（worker.extra.orchestrator_run_id = 父 run），唯一缺口是**无界深度**——加 `delegation_depth` 守卫（默认 2）。

**Tech Stack:** Rust（`nomifun-app` state.rs、`nomifun-orchestrator` engine.rs/worker.rs/run_service.rs、`nomifun-gateway` caps_orchestrator.rs），cargo-nextest。无 FE、**无迁移**（复用 `orch_runs.total_tokens` + `COUNT(orch_run_tasks)` + 现有 `orch_run_tasks.pattern_config` JSON + 会话 `extra` JSON）。

## Global Constraints

- **测试**：`cargo nextest run -p nomifun-orchestrator -p nomifun-gateway`；改 compose_lead_receipt/reporter 后**必 `cargo check -p nomifun-app`**（跨 crate）。无 `| tail`。
- **载重（最高风险）**：自主循环的 runaway 防护**必须是网关追加口的硬闸**（`run_is_appendable` 折进预算/任务数上限），不是提示词——提示词是 cooperative，LLM 可绕；硬闸用**累计持久 `total_tokens` + `list_tasks` 计数** vs 常量。**先落硬闸(Task1)，再接自主提示(Task2)。**
- **无迁移**：所有计数复用既有列/JSON。可选迁移 030（durable round/no-progress）**延后**，非 v1 依赖。
- **只读受限角色继续禁委派**（`role_allowed_tools` 返回 Some → `desktopGateway=false`，天然无网关工具，零改）。
- **不破坏既有**：`autonomy` 门只 `interactive` 特殊，`autonomous`==`running`（保 boot-resumable）不动；`LeadReporter::report` trait 签名 + 3 impl 不动（给 reporter 加 `run_repo` 字段而非改签名，零 call-site/test churn）。
- **Git**：分支 `feat/phase1-reliability-shared-context`（续，叠栈）。Task2 ∥ Task3 worktree 并行（见执行说明；worktree 默认从 main→agent 须 reset 到当前 base）。提交前 `git pull --rebase`。

## 常量（新增，engine.rs tunables 区 ~245-286）
```rust
/// 自主编排 run 的硬 token 上限:累计 total_tokens 达此即拒绝追加(确定性 backstop)。
pub const AUTONOMOUS_TOKEN_BUDGET: i64 = 2_000_000;
/// 自主编排 run 的硬任务数上限(轮次/扇出):list_tasks 计数达此即拒绝追加。兼作嵌套聚合封顶。
pub const AUTONOMOUS_MAX_TASKS: usize = 60;
/// 连续无新 done 节点的软阈值:达此 reporter 改发「收尾」提示(硬闸仍是 token/task cap)。
pub const NO_PROGRESS_LIMIT: u32 = 2;
/// 嵌套委派深度上限:root lead=0,其 worker=1,再委派=2,depth-2 worker 拒绝再委派。
pub const DELEGATION_DEPTH_LIMIT: u32 = 2;   // (置于 caps_orchestrator.rs)
```

## 任务依赖 / 并行
```
Task 1 (硬预算闸: run_is_appendable/网关追加口 + engine 常量) ── 基础,串行,先落(runaway backstop)
   ├──> Task 2 (自主提示: nomifun-app state.rs reporter+compose_lead_receipt) ──┐ 文件不相交
   └──> Task 3 (嵌套深度: worker.rs+engine dispatch+caps_orchestrator.rs)      ──┘ → worktree 并行后合并
```
Task 2(state.rs）与 Task 3(worker/engine/caps）文件不相交、均从 Task1 合入后开 → 并行 worktree。

---

### Task 1: 硬预算闸（自主 runaway backstop）

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/engine.rs`（3 常量 AUTONOMOUS_TOKEN_BUDGET/MAX_TASKS/NO_PROGRESS_LIMIT）
- Modify: `crates/backend/nomifun-gateway/src/caps_orchestrator.rs`（`run_is_appendable` 折进预算闸；两个追加口 `nomi_run_add_tasks` handler + `spawn` 收敛分支拒绝并返清晰错误）
- Test: `caps_orchestrator.rs` mod tests（或 orchestrator 若闸在 run_service）

**Interfaces:**
- Produces: `run_is_appendable`（现 = 存在且非 cancelled）扩为**且未超预算**：读 run（`total_tokens`）+ `list_tasks` 计数；`total_tokens >= AUTONOMOUS_TOKEN_BUDGET || task_count >= AUTONOMOUS_MAX_TASKS` → 不可追加。仅对 `autonomy == "autonomous"` 的 run 应用预算闸（supervised/手动追加不受此限——除非你要全局限；默认只限 autonomous）。
- Consumes: 既有 `read_conversation_run_id`、run service get/get_detail、`list_tasks`。
- 追加口拒绝时返回明确 tool 错误：`"autonomous budget exhausted (tokens/rounds); finalize and report to the user"`，且**不重臂**（run 保持终态 → 循环干净停止）。

- [ ] **Step 1: 写失败测试**：一个 autonomy=autonomous 的 run，`total_tokens` 或 task 数超上限 → `run_is_appendable` 返 false / `nomi_run_add_tasks` 返预算错误、不追加。用小常量或注入可配上限触发（若常量硬编码，测试构造超限 run 状态）。
- [ ] **Step 2: 运行确认失败。**
- [ ] **Step 3: 加 3 常量（engine.rs）。**
- [ ] **Step 4: `run_is_appendable` 折进预算闸**（对 autonomous run：超 token/task cap → 不可追加）；两个追加口在 append 前校验，超限返错误不追加不重臂。
- [ ] **Step 5: 跑测试 + 跨 crate**：`cargo nextest -p nomifun-gateway -p nomifun-orchestrator` 绿；`cargo check -p nomifun-app` clean。
- [ ] **Step 6: 提交** `feat(orch): 自主编排硬预算闸(token/任务数上限折进 run_is_appendable，防 runaway)`。

---

### Task 2: 自主提示（reporter 感知 autonomy/目标/预算）【∥ Task 3】

**Files:**
- Modify: `crates/backend/nomifun-app/src/router/state.rs`（`OrchestratorLeadReporter` 加 `run_repo: Arc<dyn IRunRepository>` 字段 + 装配 ~1159；`report` best-effort `run_repo.get_run`；`compose_lead_receipt` 加 autonomy/goal/budget-aware 分支 + in-memory 无进展 streak DashMap）
- Test: state.rs mod tests（若有）/ 至少 `cargo check`

**Interfaces:**
- Consumes: Task 1 的预算语义（判断「剩余预算/轮次」用于提示）；`run.autonomy`/`run.goal`/`run.total_tokens`/`list_tasks`。
- Produces: autonomous run 的 `Completed`/`Failed` 回执改为**自评提示**（见 build sheet §1a）：给目标+本轮 brief+预算余量，指示「达成→汇总收尾不追加；未达成且有预算→`nomi_run_add_tasks` 追加继续；预算尽→汇总交用户不追加」。非 autonomous run 回执**不变**（现有「仅汇总」措辞）。无进展 streak（in-memory `DashMap<run_id,{last_done,streak}>`）达 `NO_PROGRESS_LIMIT` → 发收尾提示。
- **不改 `LeadReporter::report` trait 签名**（加字段到 struct，`report` 内自取 run_repo）。

- [ ] **Step 1: 写测试**（可行则）：autonomous run 的 Completed 回执含目标+「自评/追加/收尾」语义、非 autonomous 不变。若 state.rs 难单测则以 `cargo check` + 人工核对措辞为准。
- [ ] **Step 2-4: reporter 加 run_repo 字段 + 装配；`report` 取 run 行；`compose_lead_receipt` 加 autonomy/goal/budget 分支 + streak。**
- [ ] **Step 5: `cargo check -p nomifun-app` clean + `cargo nextest -p nomifun-orchestrator`（若 reporter 测试在此）绿。**
- [ ] **Step 6: 提交** `feat(app): 自主编排回执感知目标/预算(自评达成→收尾,未达→追加,尽→交用户)`。

---

### Task 3: 受控嵌套深度守卫（W7d）【∥ Task 2】

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/worker.rs`（`build_worker_extra` +`delegation_depth: u32` 参数 + json 键 `orchestrator_delegation_depth`）
- Modify: `crates/backend/nomifun-orchestrator/src/engine.rs`（dispatch 处读 task 的 `pattern_config.delegation_depth` 默认 0，传给 build_worker_extra）
- Modify: `crates/backend/nomifun-gateway/src/caps_orchestrator.rs`（`read_conversation_delegation_depth`；两个追加口 depth 守卫 + 给追加 task 盖 `pattern_config.delegation_depth = caller+1`；`DELEGATION_DEPTH_LIMIT` 常量）
- Test: worker.rs + caps_orchestrator.rs mod tests

**Interfaces:**
- 前提（已工作，勿改）：worker 的 `extra.orchestrator_run_id` = **父 run**（workers 与 lead 共 run，仅 task_id 异）→ worker 调 `nomi_run_add_tasks` 追加同级到父 run。只读受限角色 `desktopGateway=false` 已禁。
- Produces：`build_worker_extra` 盖 `orchestrator_delegation_depth`（root lead 无键=0）；per-task 深度存既有 `pattern_config` JSON（`{"delegation_depth":N}`，与 `{"group":…}` 合并同对象）；引擎 dispatch 读 task pattern_config 深度传入；网关 `read_conversation_delegation_depth`（读 `conv.extra["orchestrator_delegation_depth"]` 默认 0）；追加口若 `caller_depth+1 > DELEGATION_DEPTH_LIMIT(2)` 返错误不追加，否则给每个追加 PlannedTask 盖 `pattern_config.delegation_depth = caller_depth+1`（守卫沿链下传）。
- 深度梯：root=0 → worker=1 → 再委派=2 → depth-2 worker 拒绝再委派。

- [ ] **Step 1: 写测试**：`build_worker_extra` 盖 depth 键；caller_depth+1 超 LIMIT 时追加口拒绝；未超时追加 task 盖 depth+1。
- [ ] **Step 2-4: 三处协调改动**（build_worker_extra 参数+键；engine dispatch 读 pattern_config.delegation_depth 传入；网关 read_conversation_delegation_depth + 两追加口守卫 + 盖 depth）。注意 pattern_config JSON 与既有 group 用法合并。
- [ ] **Step 5: 跑测试 + 跨 crate**：`cargo nextest -p nomifun-orchestrator -p nomifun-gateway` 绿；`cargo check -p nomifun-app` clean。
- [ ] **Step 6: 提交** `feat(orch): 受控嵌套 delegation_depth 守卫(worker 追加父run,深度<=2)`。

---

## 执行说明（并行）
- Task 1 串行先落（runaway 硬闸，最高风险，先行）。
- Task 2 ∥ Task 3 worktree 隔离并行：Task2=nomifun-app state.rs；Task3=worker.rs+engine.rs+caps_orchestrator.rs——文件不相交。**从 Task1 合入后的栈顶开 worktree（agent 须 reset 到该 base，勿默认 main）。** 各自评审后依次合并，合并后复合 `cargo nextest -p nomifun-orchestrator -p nomifun-gateway` + `cargo check -p nomifun-app`。
  > 注:Task3 与 Task1 都动 caps_orchestrator.rs+engine.rs(不同函数/区域)，故 Task3 须从 Task1 后开;与 Task2(state.rs)不相交可并行。

## 自审（Self-Review）
**1. Spec 覆盖（Phase3 设计 §3 W7c/W7d + F1/F2）：** W7c 自主循环(emergent) → Task1(硬闸)+Task2(提示)；F1 多重退出=自评达成(Task2 提示)+token/task 预算(Task1 硬闸)+无进展 streak(Task2 软)+用户中止(既有 cancel)。W7d 嵌套 → Task3；F2 深度2/并入父图(已工作)/只读禁委派(已工作)。✅ 迁移 030 durable no-progress 延后(记录,非 v1)。
**2. Placeholder 扫描：** 无 TBD。build sheet 给了确切 seam(state.rs:990/999/1035/1159, caps:459/896/1058, worker:488/502/530, engine:664/572, run_service:463)。常量给了默认值。
**3. 类型一致性：** `AUTONOMOUS_TOKEN_BUDGET:i64`/`AUTONOMOUS_MAX_TASKS:usize`/`NO_PROGRESS_LIMIT:u32`/`DELEGATION_DEPTH_LIMIT:u32` 各处一致；`delegation_depth:u32` worker 参数/json 键/pattern_config JSON/网关读取一致；`run_is_appendable` 折闸后两追加口共用。
**4. 跨 crate（Phase1a 教训）：** Task2 改 reporter(加字段,不改 trait 签名→零 impl churn)；Task1/3 改 caps+engine+worker。每任务 `cargo check -p nomifun-app`。
**5. 载重不变量：** runaway 硬闸在网关追加口(LLM 绕不过)+累计持久 total_tokens+task 计数；深度双闸(DELEGATION_DEPTH_LIMIT + AUTONOMOUS_MAX_TASKS 聚合封顶)；终止保证(超闸→不重臂→终态)；boot-resume 复用(autonomous==running)。

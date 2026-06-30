# 多 Agent 智能编排引擎 · P2 实施计划（真并行执行 + 取消传播）

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]`.

**Goal:** 让 RunEngine 从「串行一次一个就绪任务」升级为「**真并行**，受并发上限约束，依赖仍严格」；worker 会话 id 在创建即上报（前端节点立即可看实时转录）；停止/取消 Run 时**传播取消到在飞 worker 会话**（以 Finish(Cancelled) 收尾）；worker 获得 Run 工作目录。实时流式画布 + 节点转录面板已在 P1b 交付（WS 驱动 useRunLive 刷新），P2 不重做。

**Architecture:** 改造 `nomifun-orchestrator` 的 `RunEngine::run_loop`（engine.rs）+ `ConversationWorkerRunner`（worker.rs）+ `RunEngineDeps` + app `build_orchestrator_state`。并发上限来自 `run.max_parallel ?? fleet_snapshot.max_parallel ?? 全局默认`。在飞 worker 用 `DashMap<task_id, conv_id>` 跟踪；取消经注入的 cancel 钩子（`ConversationService::cancel` by conv_id）。WorkerRunner 新增 `on_started(conv_id)` 回调（创建会话后立即触发）→ 引擎记录在飞 + 立即 `update_task(conversation_id)`（前端实时转录）。

**Spec：** `docs/superpowers/specs/2026-06-26-multi-agent-orchestrator-design.md` §4.5（并发模型）、§7（取消令 worker 以 Finish(Cancelled) 收尾）。承接 P1a carry-forward（取消传播、workspace_dir）。

## Global Constraints
- **并发但依赖严格**：只调度 `list_ready_tasks`（deps 全 done）的任务；在飞≤cap；完成即重评就绪集再补位；全终态→完成聚合。**不破坏** P1a 的确定性 + 无 busy-spin + generation 守卫 + boot-resume。
- **每会话仍串行单回合**（不破坏 TurnClaim）；并行发生在不同 worker 会话之间。
- **取消令在飞 worker 以 Finish(Cancelled) 收尾**（经 ConversationService::cancel，非 Error）；worker await_turn 见 is_processing 清零即返 ok=false。
- **worker conversation_id 创建即落 task 行**（on_started 回调 → update_task），前端节点立即可开转录；i64。
- 引擎复刻/沿用 AutoWork Orchestrator 形态；mock trait（MockPlanProducer/MockWorkerRunner）单测；端到端 mock。
- 测试只跑触碰 crate（`cargo nextest run -p nomifun-orchestrator`/`-p nomifun-app`）；**禁 cargo fmt**；app 必编过。
- 提交：feature 分支 `feat/multi-agent-orchestrator`；每任务末提交；**禁合并 main**。

## File Structure（P2）
- 修改 `crates/backend/nomifun-orchestrator/src/worker.rs`（WorkerRunner trait + ConversationWorkerRunner：on_started 回调）。
- 修改 `crates/backend/nomifun-orchestrator/src/engine.rs`（RunEngineDeps + 并行 run_loop + 在飞 map + 取消传播）。
- 修改 `crates/backend/nomifun-orchestrator/src/run_service.rs`（若 cancel 需联动）。
- 修改 `crates/backend/nomifun-app/src/router/state.rs`（build_orchestrator_state：注入 cancel 钩子 + 并发配置 + workspace 解析）。
- 修改/扩展 `crates/backend/nomifun-app/tests/orchestrator_run_e2e.rs`（并行 + 取消）。

---

## Task 1: WorkerRunner 早报 conv_id（on_started 回调）+ 可取消基础

**Files:** Modify `worker.rs`；Test 内联。

**Interfaces produced（改造 trait — 这是 breaking change，Task 2 引擎随之更新调用）:**
```rust
#[async_trait::async_trait]
pub trait WorkerRunner: Send + Sync {
    /// on_started 在 worker 会话创建后、send/await 之前被调用一次(传 conversation_id),
    /// 供引擎记录在飞会话 + 立即落 task.conversation_id(前端实时转录)。
    async fn run(
        &self, member: &FleetMember, workspace_dir: Option<&str>,
        run_id: &str, task_id: &str, brief: &str, task_spec: &str,
        timeout: Duration, on_started: Box<dyn FnOnce(i64) + Send>,
    ) -> Result<WorkerOutcome, AppError>;
}
```
- `ConversationWorkerRunner::run`：`create()` 得到 conv 后**立即 `on_started(conv.id)`**，再 send_message + await_turn + read_final_text（其余配方不变）。
- `MockWorkerRunner`：也调用 `on_started(fixed_conv_id)` 后返回固定 outcome（供 Task 2 引擎测试在飞跟踪 + 可选延迟）。给 MockWorkerRunner 加一个可选 `delay: Duration`（默认 0；Task 2 并发测试用它制造重叠窗口）。

参照模板：当前 `worker.rs`（P1a，nomi_agent_run 配方）。

- [ ] **Step 1: 改 trait + 两个 impl 测试（失败优先）** — ConversationWorkerRunner（对 Mock AgentInstance 或仅断言 on_started 在 create 后、await 前被调用一次 + 传对 conv_id）；MockWorkerRunner（on_started 调用 + delay 生效）。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-orchestrator worker`。
- [ ] **Step 3: 实现** on_started（ConversationWorkerRunner 在 create 后调用；MockWorkerRunner 调用 + delay）。
- [ ] **Step 4: 跑确认通过** + `cargo build -p nomifun-orchestrator`（注意：引擎现有调用点会因签名变更编译失败——本任务**仅改 worker.rs + 其测试**，引擎调用点的修复在 Task 2；若 `cargo build -p nomifun-orchestrator` 因 engine.rs 调用点报错，这是预期的，Task 2 修；本任务用 `cargo build -p nomifun-orchestrator --lib` 是否能过取决于 engine 是否同 crate——engine 同 crate,故本任务需同时把 engine.rs 的调用点临时适配[传一个空 on_started Box::new(|_|{})]以保持 crate 编译,Task 2 再做真并行)。**决策**：本任务把 engine 调用点改为传 `Box::new(|_| {})` 占位(保持串行+编译绿),Task 2 替换为真并行+在飞记录。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): WorkerRunner on_started 早报 conv_id"`

---

## Task 2: 并行 RunEngine 调度器（并发上限）+ workspace_dir 注入

**Files:** Modify `engine.rs`（RunEngineDeps + 并行 run_loop + 在飞 map + workspace 解析）；Test 内联。 **Model: opus**（并发正确性）。

**Interfaces produced:**
```rust
pub struct RunEngineDeps {
    pub run_repo: Arc<dyn IRunRepository>,
    pub worker: Arc<dyn WorkerRunner>,
    pub emitter: OrchestratorRunEventEmitter,
    pub worker_timeout: Duration,
    pub default_max_parallel: usize,          // 全局默认(如 4)
    pub ws_repo: Arc<dyn IOrchWorkspaceRepository>, // 解析 run→workspace→workspace_dir
    // (cancel 钩子在 Task 3 加)
}
```
**并行 run_loop（核心）：**
```
let cap = resolve_cap(run);  // run.max_parallel ?? fleet_snapshot.max_parallel ?? default_max_parallel, clamp>=1
let inflight: HashMap<task_id, JoinHandle/conv_id> (or FuturesUnordered);
loop {
  if cancelled { break }
  // 补位:在 inflight.len() < cap 时,取就绪任务并 spawn,直到无就绪或满 cap
  let ready = run_repo.list_ready_tasks(run_id).await?;
  for task in ready.iter().filter(not already inflight) .take(cap - inflight.len()) {
     mark running + emit; resolve member from fleet_snapshot via assignment;
     resolve workspace_dir = ws_repo.get(run.workspace_id).workspace_dir;
     spawn worker.run(member, workspace_dir, run_id, task.id, brief, spec, timeout, on_started=记录 conv_id + update_task(conversation_id) + emit);
     inflight.insert(task.id, handle);
  }
  if inflight.is_empty() {
     // 无在飞且无就绪 → 判定终态(全 done/skipped→completed+聚合; 有 failed→failed) break;
  }
  // 等任一在飞完成(select_next / FuturesUnordered.next),处理 outcome:
  //   ok→update_task(done, conversation_id, output_summary)+emit; 失败→failed+emit;
  //   从 inflight 移除; 循环继续(重评就绪集补位)。
}
```
- **并发正确性**：用 `futures::stream::FuturesUnordered`（或 `tokio::task::JoinSet`）持有在飞 worker future；`tokio::select!` 在「任一完成」与「cancel notified」间择一。**绝不** busy-spin（无就绪且有在飞时 await 在飞完成；无就绪且无在飞时判终态 break）。**依赖严格**：每轮重查 list_ready_tasks（completion 自然解阻塞），已在飞任务不重复 spawn。
- **brief 注入上游产物**：组装 brief 时纳入已 done 上游任务的 output_summary（同 P1a）。
- **workspace_dir 注入**：`ws_repo.get(run.workspace_id)?.workspace_dir` 传给 worker（修 P1a 的 None 桩）。
- 在飞 map 同时存 conv_id（on_started 回调写入）供 Task 3 取消。

参照模板：当前 `engine.rs`（P1a 串行 run_loop）；AutoWork Orchestrator；`futures::FuturesUnordered`/`tokio::JoinSet` 语义。

- [ ] **Step 1: 写并发集成测试（失败优先，全 mock）** — DAG: A(无依赖), B(无依赖), C(依赖 A+B)。MockWorkerRunner delay=100ms。cap=2。断言：A、B **并发**执行（用计数器/时间戳证重叠：总耗时 ≈100ms 而非串行 200ms+；或记录 max 并发=2），C 在 A、B 都 done 后才跑；run→completed，全 done，output_summary 落库。再测 cap=1 退化为串行（A→B→C 序）。再测 workspace_dir 透传（MockWorkerRunner 记录收到的 workspace_dir == run 的 workspace_dir）。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-orchestrator engine`（现串行,并发断言失败）。
- [ ] **Step 3: 实现并行 run_loop + RunEngineDeps 扩展 + workspace 解析 + on_started 在飞记录/落库**。
- [ ] **Step 4: 跑确认通过** + `cargo build -p nomifun-orchestrator`。**关键**：复核无 busy-spin（无就绪+有在飞→await；无就绪+无在飞→break）、依赖严格（C 不早跑）、cap 生效。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): RunEngine 真并行调度(并发上限)+workspace_dir 注入"`

---

## Task 3: 取消传播到在飞 worker + app 接线 + 集成测试

**Files:** Modify `engine.rs`（cancel 钩子 + stop 时取消在飞 conv）、`run_service.rs`（cancel 联动若需）、`state.rs`（build_orchestrator_state 注入 cancel 钩子 + default_max_parallel + ws_repo）、`tests/orchestrator_run_e2e.rs`。

**设计:**
- `RunEngineDeps` 加 `cancel_conversation: Arc<dyn Fn(i64) + Send + Sync>`（或一个小 trait `ConversationCanceller { async fn cancel(&self, conv_id: i64); }`）。app 注入一个调用 `ConversationService::cancel(SYSTEM_USER_ID, conv_id.to_string(), ..., &task_manager)` 的实现（读 service.rs 确认 cancel 签名：user_cancel 戳 + agent.cancel,幂等;无活 agent 时 no-op）。
- `RunEngine::stop(run_id)`：设 cancelled 标志 + abort loop（现有）**并** 取消该 run 所有在飞 conv（遍历在飞 map 的 conv_id 调 cancel_conversation）。worker 的 await_turn 见 is_processing 清零 → 返 ok=false → 任务标 failed/cancelled（P2：取消的 run，任务收尾不必标 failed,可标 cancelled 或留 running 由 run=cancelled 覆盖——取简单:run=cancelled 即可,在飞任务被取消后自然 await 返回,引擎因 cancelled 不再处理 outcome)。
- **app 接线**（build_orchestrator_state）：RunEngineDeps 加 `default_max_parallel`(如 4)、`ws_repo`、`cancel_conversation`(包 ConversationService::cancel)。其余沿用。
- **集成测试**（mock，nomifun-app 或 orchestrator crate）：(a) 并行 run 经真 wiring 跑到 completed（mock planner+worker）；(b) cancel 中途 → run=cancelled，在飞 worker 被调用 cancel（mock canceller 记录被调用的 conv_id 非空）。**app 必编过**。

参照模板：`ConversationService::cancel`（service.rs）；AutoWork Orchestrator stop 的 task_manager.get_task().cancel()（orchestrator.rs:246-255）；P1a build_orchestrator_state。

- [ ] **Step 1: 写取消传播测试（失败优先）** — engine: 用 MockWorkerRunner（长 delay + on_started 报 conv_id）+ mock canceller（记录被取消的 conv_id）。start run → 等任务进 running（conv 已报）→ stop → 断言 mock canceller 收到在飞 conv_id，run=cancelled。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-orchestrator engine`（cancel propagation）。
- [ ] **Step 3: 实现 cancel 钩子 + stop 取消在飞 + RunEngineDeps 扩展**。
- [ ] **Step 4: app 接线** build_orchestrator_state 注入 cancel_conversation(ConversationService::cancel) + default_max_parallel + ws_repo。
- [ ] **Step 5: `cargo build -p nomifun-app`（关键闸）+ 集成测试** — 并行 run 跑通 + cancel 传播；扩展 orchestrator_run_e2e。
- [ ] **Step 6: 提交** `git commit -m "feat(orchestrator): 取消传播到在飞 worker + 并行引擎 app 接线"`

---

## Self-Review（对照 spec §4.5/§7 P2 切片）
**覆盖：** §4.5 真并行(并发上限,worker 间并行,每会话仍串行)→Task 2;§7 取消令 worker Finish(Cancelled)→Task 3;P1a carry-forward workspace_dir→Task 2,cancel 传播→Task 3。**P2 不含**：能力 Router 打分(P3)、自主三级闸(P3)、provider-rate 细粒度软上限(可选,先用 cap+default)、retry(P3)。实时画布/转录已 P1b 交付。
**占位符：** 无 TBD;「取消的任务收尾用 run=cancelled 覆盖不强标 failed」「provider-rate 软上限延后」是有意 P2 边界。
**类型一致：** WorkerRunner.run 新签名(on_started)(Task1)→engine 调用(Task2)一致;RunEngineDeps 字段(Task2/3)→build_orchestrator_state(Task3)一致;cancel 钩子签名 engine↔app 一致。

## Execution Handoff
波次：Task1→Task2(opus,并发)→Task3(取消+接线)。SDD 每任务两阶评审+fix+记账。真机并行/取消验收需 provider,留用户；CI 用 mock 延迟 worker 证并发+取消。

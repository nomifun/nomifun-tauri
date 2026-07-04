# Phase 1a — 工作流可靠性 (W4) 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让编排 run 在节点失败/超时时能有界自愈、部分交付、并把永久失败中途上报 lead 修复——确保长复杂 run 尽力完工并如实汇总，而非「一个节点拖垮全部、静默卡到终态」。

**Architecture:** 全部在 `nomifun-orchestrator` 引擎内，复用现有重试地基（迁移 024 `next_retry_at` + `settle_failed_or_retry` + `RunEngineDeps` 退避字段）与失败原因持久化（迁移 027 `last_error`）。新增：超时/空回复的有界重试分类 + run 级总预算；节点 `on_fail` 策略列（迁移 029）+ 新终态 `completed_with_failures`（复用 `skip_downstream`）；永久失败时经既有 `LeadReporter` 中途回执（新 `RunOutcome::NodeFailed`）；规划器提示新增收尾验收门。零新引擎、复用 RunLocks 原子性。

**Tech Stack:** Rust（`nomifun-orchestrator`、`nomifun-db`），cargo-nextest；少量 FE（`runStatusMeta.ts` + i18n）。

## Global Constraints

- **Rust 测试**：只跑触碰 crate：`cargo nextest run -p nomifun-orchestrator`；加迁移后另跑 `cargo nextest run -p nomifun-db`。避免 `| tail` 掩盖退出码；`cargo check` 不编译 test。
- **迁移**：下一个号 = **029**（最新为 028）。`db_lifecycle.rs` 自动从 `_sqlx_migrations` 推导版本、`sqlx::migrate!()` 构建期发现目录——**无需手改 db_lifecycle 里的硬编码计数**；加完 029 跑 `cargo nextest run -p nomifun-db` 验证。提交前 `git pull --rebase` 防撞号。（[[pull-before-commit]]）
- **迁移 append-only**：`ALTER TABLE orch_run_tasks ADD COLUMN`（O(1)，不重建表），可空列，旧行读回 `NULL` = 默认策略，既有 run/plan 零回归。
- **不变量勿破**：`running ⟺ 活体 worker`；终态判定在 `RunLocks` per-run 锁内、loop 在锁内注销 handle 使 `is_running()` 与终态写原子。任何新分支必须在同一锁语义内，best-effort 外部调用（lead 回执）绝不在锁内 await。（[[orch-orphaned-running-fix]]）
- **前端**：改 `runStatusMeta.ts`/i18n 后必仓库根 `bun run gen:i18n`；`ui/` 下 `bun run typecheck` exit 0。
- **Git**：分支 `feat/phase1-reliability-shared-context`（已建，stacked on Phase 0）。逐任务提交。
- **保留不动**：既有重试地基（迁移 024）、`last_error`（迁移 027）、`skip_downstream`、`RunLocks`、`reap_stalled_runs`、boot-resume、验证聚合器 `settle_verify_task`。

## 术语 / 约定（本计划统一命名，各任务一致）

- `WorkerErrorClass`（本计划不引入枚举，改用两个 worker 布尔信号，见下）。
- 新 worker 方法 `last_error_present(conv) -> bool`：worker 会话是否存在 `content.type=="error"` 标记（默认 `false`）。配合既有 `last_error_retryable`（标记的 `error.retryable` 旗标；无标记→`false`）区分三态：
  - **Retryable 商错误**（如限流）：`last_error_retryable==true`。
  - **NonRetryable 商错误**（auth/billing/bad-request）：`last_error_present==true && last_error_retryable==false`。
  - **NoMarker（超时/空回复）**：`last_error_present==false`（worker 返回 `ok:false` 但无错误标记）。
- 新常量 `DEFAULT_MAX_TIMEOUT_RETRIES: usize = 2`（NoMarker 类的独立、更小预算）。
- 新 `RunEngineDeps.max_timeout_retries: usize`（默认 `DEFAULT_MAX_TIMEOUT_RETRIES`）。
- run 级总预算：**无状态派生**——调度重试前校验 `sum(task.attempt over run) < run_total_retry_budget`，其中 `run_total_retry_budget = max(list_tasks.len(), 1) * deps.max_worker_retries`。超预算→永久失败（防宽扇出重试风暴）。
- 节点 `on_fail` 策略：`orch_run_tasks.on_fail TEXT`，取值 `"fail_run"`（默认/NULL）| `"skip_and_continue"`。
- 新终态 `completed_with_failures`：run 所有任务已 settled（`done`/`skipped`/`failed(skip_and_continue)`）且无 `fail_run` 类硬失败、但存在 `skip_and_continue` 失败节点。
- 新 `RunOutcome::NodeFailed`（`as_str() == "node_failed"`）：中途永久失败回执用。

---

### Task 1: 超时/空回复有界重试 + run 级总预算

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/worker.rs`（`WorkerRunner` trait 加 `last_error_present`；prod impl；`RetryMockWorkerRunner` 覆写以保留既有语义——见 Interfaces）
- Modify: `crates/backend/nomifun-orchestrator/src/engine.rs`（常量 `DEFAULT_MAX_TIMEOUT_RETRIES`；`RunEngineDeps.max_timeout_retries` + 默认；重写 `settle_failed_or_retry`）
- Test: 同 `engine.rs` `#[cfg(test)] mod tests`（新增超时重试测试 + 新 `TimeoutMockWorkerRunner`）

**Interfaces:**
- Produces:
  - `WorkerRunner::last_error_present(&self, conversation_id: &str) -> bool`（默认 `false`；prod 返回是否存在 error 标记）。
  - `const DEFAULT_MAX_TIMEOUT_RETRIES: usize = 2;`
  - `RunEngineDeps.max_timeout_retries: usize`
  - 重写后的 `settle_failed_or_retry`：三态分类 + 两种预算 + run 级总预算。
- Consumes: 既有 `last_error_retryable`、`retry_backoff_ms`、`mark_task_failed`、`now_ms`、`UpdateTaskParams`（`..Default::default()`）。
- **保留既有测试语义**：`RetryMockWorkerRunner` 现覆写 `last_error_retryable → self.retryable`。本任务给它加覆写 `last_error_present → true`（它模拟「带标记的商错误」）：`retryable=true`→Retryable、`retryable=false`→NonRetryable（立即失败，`non_retryable_failure_fails_immediately_without_retry` 保持绿）。超时/空回复由新的 `TimeoutMockWorkerRunner`（`ok:false, text:None`，不覆写两方法→`last_error_present=false`）覆盖。

- [ ] **Step 1: 写失败测试**（`engine.rs` mod tests）——先加 `TimeoutMockWorkerRunner` + 两个超时重试测试

在测试模块内新增（放在 `RetryMockWorkerRunner` 附近，约 `engine.rs:5413` 后）：

```rust
    /// Simulates a worker that TIMES OUT / returns no final text with NO error
    /// marker for the first `fail_times` dispatches, then succeeds. Mirrors the
    /// production timeout/empty shape: `ok:false, text:None`, and (by NOT
    /// overriding `last_error_present`) reports no error marker → NoMarker class.
    struct TimeoutMockWorkerRunner {
        fail_times: usize,
        dispatches: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl WorkerRunner for TimeoutMockWorkerRunner {
        async fn run(
            &self,
            _member: &FleetMember,
            _workspace_dir: Option<&str>,
            _run_id: &str,
            _task_id: &str,
            _brief: &str,
            _task_spec: &str,
            _timeout: Duration,
            on_started: Box<dyn FnOnce(i64) + Send>,
        ) -> Result<WorkerOutcome, AppError> {
            on_started(900);
            let n = self.dispatches.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_times {
                Ok(WorkerOutcome { conversation_id: 900, text: None, ok: false, tokens: None })
            } else {
                Ok(WorkerOutcome { conversation_id: 900, text: Some("ok".into()), ok: true, tokens: None })
            }
        }
        // last_error_present defaults false, last_error_retryable defaults false → NoMarker.
    }

    #[tokio::test]
    async fn timeout_without_marker_retries_bounded_then_succeeds() {
        // Times out once (no error marker) then succeeds → self-heals via the
        // NoMarker timeout budget (NOT the marker-retryable path).
        let worker: Arc<dyn WorkerRunner> =
            Arc::new(TimeoutMockWorkerRunner { fail_times: 1, dispatches: Default::default() });
        let (svc, engine, run_id) = single_task_harness(worker.clone()).await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(detail.run.status, "completed", "a no-marker timeout must retry to completion");
        assert_eq!(detail.tasks[0].status, "done");
        assert_eq!(detail.tasks[0].attempt, 1, "one timeout retry bumps attempt 0 → 1");
    }

    #[tokio::test]
    async fn timeout_without_marker_exhausts_timeout_budget_then_fails() {
        // Times out more than DEFAULT_MAX_TIMEOUT_RETRIES → permanent failure.
        let worker: Arc<dyn WorkerRunner> =
            Arc::new(TimeoutMockWorkerRunner { fail_times: 99, dispatches: Default::default() });
        let (svc, engine, run_id) = single_task_harness(worker.clone()).await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(detail.run.status, "failed", "timeout budget exhausted → run fails");
        assert_eq!(detail.tasks[0].status, "failed");
        assert!(detail.tasks[0].attempt as usize >= super::DEFAULT_MAX_TIMEOUT_RETRIES,
            "attempt should reach the timeout budget");
    }
```

> `single_task_harness(worker)`：若测试模块无此单任务 harness，复用 `retry_harness` 的构造方式（`SingleTaskPlanProducer` + `engine_deps.retry_backoff_base = Duration::ZERO`）新建一个返回 `(svc, engine, run_id)` 的小 helper（放在 `retry_harness` 旁），或直接内联 `retry_harness(0, false, 3)` 的骨架并换 worker。实现 Step 时按测试模块实际 helper 命名对齐。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo nextest run -p nomifun-orchestrator timeout_without_marker`
Expected: FAIL——当前超时（NoMarker）被判不可重试，`timeout_without_marker_retries_bounded_then_succeeds` 会得到 `failed` 而非 `completed`（或编译错误 `DEFAULT_MAX_TIMEOUT_RETRIES` 未定义）。

- [ ] **Step 3: 加常量 + deps 字段 + worker 信号**

`engine.rs`（常量区，`DEFAULT_MAX_WORKER_RETRIES` 附近 ~230）：
```rust
/// Independent, smaller budget for NO-MARKER failures (timeout / empty reply):
/// a worker that returns `ok:false` with no persisted error marker. Bounded
/// separately from provider-marker retries so a genuinely stuck node cannot loop.
pub const DEFAULT_MAX_TIMEOUT_RETRIES: usize = 2;
```
`RunEngineDeps`（字段区 ~363，紧邻 `max_worker_retries`）：
```rust
    /// Max auto-retries for a NO-MARKER failure (timeout / empty reply). See
    /// [`DEFAULT_MAX_TIMEOUT_RETRIES`]. 0 disables timeout auto-retry.
    pub max_timeout_retries: usize,
```
`RunEngineDeps::new` 默认（~396）：`max_timeout_retries: DEFAULT_MAX_TIMEOUT_RETRIES,`

`worker.rs`（`WorkerRunner` trait，`last_error_retryable` 默认方法旁 ~117）：
```rust
    /// Whether the worker conversation carries ANY `content.type=="error"` marker.
    /// Distinguishes a NonRetryable provider error (marker present, retryable=false)
    /// from a NoMarker timeout/empty reply (no marker). Default `false` (no marker).
    async fn last_error_present(&self, _conversation_id: &str) -> bool {
        false
    }
```
`worker.rs`（prod impl，`last_error_retryable` prod 覆写旁 ~180）：
```rust
    async fn last_error_present(&self, conversation_id: &str) -> bool {
        self.read_latest_error_present(conversation_id).await
    }
```
并加 reader（在 `read_latest_error_retryable` 旁）复用同一取数逻辑，判断是否存在 `content.type=="error"` 标记（返回 `bool`；实现镜像 `error_retryable_flag` 的取数但只判存在性）。示例 helper（自由函数，放在 `error_retryable_flag` 旁 ~528）：
```rust
fn latest_error_present(v: &Value) -> bool {
    match v {
        Value::Array(arr) => arr.iter().any(|e| error_marker_present(e)),
        _ => error_marker_present(v),
    }
}
fn error_marker_present(v: &Value) -> bool {
    v.as_object()
        .and_then(|o| o.get("content"))
        .and_then(|c| c.get("type"))
        .and_then(Value::as_str)
        == Some("error")
}
```
（`read_latest_error_present` 与 `read_latest_error_retryable` 同样拉取会话最近消息后调 `latest_error_present`。实现时复用现有取数函数、避免重复 I/O 逻辑。）

`worker.rs`（`RetryMockWorkerRunner`）：加覆写以保留既有语义——它模拟「带标记的商错误」：
```rust
    async fn last_error_present(&self, _conversation_id: &str) -> bool {
        true
    }
```

- [ ] **Step 4: 重写 `settle_failed_or_retry`（三态 + 两预算 + run 级总预算）**

替换 `engine.rs:1632-1687` 的 `settle_failed_or_retry` 为：
```rust
async fn settle_failed_or_retry(
    deps: &Arc<RunEngineDeps>,
    run_id: &str,
    task_id: &str,
    conv_from_outcome: Option<i64>,
    tokens: Option<i64>,
) {
    let task = deps.run_repo.get_task(task_id).await.ok().flatten();
    let attempt = task.as_ref().map(|t| t.attempt).unwrap_or(0);
    let conv = conv_from_outcome.or_else(|| task.as_ref().and_then(|t| t.conversation_id));

    // Three-way classification against the worker conversation:
    //   Retryable  = provider marker with retryable=true (e.g. rate limit)
    //   NoMarker   = ok:false with no error marker (timeout / empty reply)
    //   Otherwise  = NonRetryable (auth/billing/bad-request marker, or Err w/ no conv)
    let (retryable_marker, has_marker) = match conv {
        Some(c) => {
            let s = c.to_string();
            (deps.worker.last_error_retryable(&s).await, deps.worker.last_error_present(&s).await)
        }
        None => (false, true), // Err outcome (no conversation) → treat as NonRetryable
    };
    let (should_retry, budget) = if retryable_marker {
        (true, deps.max_worker_retries)
    } else if !has_marker && conv.is_some() {
        (true, deps.max_timeout_retries) // NoMarker: timeout / empty reply
    } else {
        (false, 0) // NonRetryable marker
    };

    // Run-level total-retry budget: bound wide fan-outs so many independently
    // backing-off nodes cannot storm the provider. Stateless: sum of persisted
    // attempts across the run vs tasks.len() * max_worker_retries.
    let within_run_budget = match deps.run_repo.list_tasks(run_id).await {
        Ok(tasks) => {
            let used: i64 = tasks.iter().map(|t| t.attempt).sum();
            let cap = (tasks.len().max(1) as i64) * (deps.max_worker_retries as i64);
            used < cap
        }
        Err(_) => true, // fail-open on a transient read error; per-node budget still bounds it
    };

    if should_retry && (attempt as usize) < budget && within_run_budget {
        let next = now_ms() + retry_backoff_ms(deps.retry_backoff_base, attempt);
        let _ = deps
            .run_repo
            .update_task(
                task_id,
                UpdateTaskParams {
                    status: Some("pending".to_string()),
                    attempt: Some(attempt + 1),
                    next_retry_at: Some(Some(next)),
                    tokens: tokens.map(Some),
                    ..Default::default()
                },
            )
            .await;
        deps.emitter.emit_task_status(run_id, task_id, "pending");
        info!(run_id, task_id, attempt = attempt + 1, no_marker = !has_marker,
            "Run loop: scheduling bounded auto-retry");
    } else {
        mark_task_failed(deps, run_id, task_id, conv_from_outcome, tokens).await;
    }
}
```

- [ ] **Step 5: 运行测试确认通过 + 既有重试测试不回归**

Run: `cargo nextest run -p nomifun-orchestrator`
Expected: PASS——新增两测试绿；既有 `retryable_failure_auto_retries_then_succeeds`、`retryable_failure_exhausts_bounded_retries_then_fails`、`non_retryable_failure_fails_immediately_without_retry`、`failed_task_persists_last_error`、lead-report 测试全部保持绿（`RetryMockWorkerRunner` 现 `last_error_present=true` 保语义；`AlwaysFailWorker` 无覆写→NoMarker，但 `reporter_stack(worker,0)` 用 `max_retries=0`，且 `max_timeout_retries` 默认 2——**注意**：`failed_run_reports_once_to_lead`/`failed_task_persists_last_error` 用 `AlwaysFailWorker`（NoMarker）+ `max_retries=0`，现在 NoMarker 预算是 `max_timeout_retries=2` 而非 `max_retries`，会导致这些测试的 run 先重试 2 次才失败——**需在这些测试的 harness 里把 `max_timeout_retries` 也设为 0**，或让 `AlwaysFailWorker` 覆写 `last_error_present=true`（模拟带标记的非重试错误）。**实现 Step 时：给 `AlwaysFailWorker` 覆写 `last_error_present -> true` 最省事（它本就代表「明确失败」），使其走 NonRetryable 立即失败路径，保持既有断言与 dispatch 计数。**）
Run: `cargo nextest run -p nomifun-orchestrator retry ; cargo nextest run -p nomifun-orchestrator lead`
Expected: 全绿。

- [ ] **Step 6: 提交**
```bash
git add crates/backend/nomifun-orchestrator/src/worker.rs crates/backend/nomifun-orchestrator/src/engine.rs
git commit -m "feat(orch): 超时/空回复有界重试 + run 级总重试预算"
```

---

### Task 2: 节点 `on_fail` 策略 + `completed_with_failures` 终态（部分交付）

**Files:**
- Create: `crates/backend/nomifun-db/migrations/029_task_on_fail.sql`
- Modify: `crates/backend/nomifun-db/src/models/orchestrator.rs`（`OrchRunTaskRow` 加 `on_fail`）
- Modify: `crates/backend/nomifun-db/src/repository/sqlite_orch_run.rs`（task SELECT 补 `on_fail`；`UpdateTaskParams` 若需写入则补——本任务只读 `on_fail`，创建时由 plan 落库，见下）
- Modify: `crates/backend/nomifun-orchestrator/src/engine.rs`（永久失败时按策略 `skip_downstream`；终态判定分 `completed_with_failures`）
- Modify: `crates/backend/nomifun-orchestrator/src/run_service.rs`（3 处 re-arm 终态门 加 `completed_with_failures`）
- Modify: `crates/backend/nomifun-db/src/repository/`（persist_dag 落 `on_fail`——若 plan 提供）
- Test: `engine.rs` mod tests

**Interfaces:**
- Produces: 列 `orch_run_tasks.on_fail TEXT`；`OrchRunTaskRow.on_fail: Option<String>`；终态字符串 `"completed_with_failures"`；引擎按 `on_fail` 分流。
- Consumes: 既有 `skip_downstream(deps, run_id, from_task_id, dep_edges)`、`list_deps`、`finish_run`、终态判定块（`engine.rs:1201-1279`）。
- 默认策略：`on_fail == None || "fail_run"` → 硬失败（现状）；`"skip_and_continue"` → 跳过下游 + run 可 `completed_with_failures`。**本任务不引入 UI 选策略**——`on_fail` 由后续（Phase 2/规划器）设置；本任务先建列 + 引擎语义 + 读取，默认 `fail_run` 保证零回归。

- [ ] **Step 1: 迁移 029**（Create）
```sql
-- 029 编排节点失败策略。append-only,基线不动。
-- 给 orch_run_tasks 加可空列 on_fail:节点永久失败时的处置策略。
--   NULL / "fail_run"        = 默认:任一必需节点永久失败即整 run 判 failed(现状,零回归)。
--   "skip_and_continue"      = 跳过该节点的传递性下游(标 skipped),其余独立分支照常跑完;
--                              run 全部 settled 且无 fail_run 硬失败时,判 completed_with_failures。
-- 纯 ADD COLUMN(O(1));旧行读回 NULL = fail_run —— 既有 run/plan 零回归。
ALTER TABLE orch_run_tasks ADD COLUMN on_fail TEXT;
```

- [ ] **Step 2: 跑迁移测试确认 schema 干净**

Run: `cargo nextest run -p nomifun-db`
Expected: PASS（`db_lifecycle` 自动发现 029 并应用；无硬编码计数需改）。

- [ ] **Step 3: `OrchRunTaskRow` 加字段**（`models/orchestrator.rs`，`last_error` 之后、`created_at` 之前）
```rust
    #[serde(default)]
    pub on_fail: Option<String>,
```
并在 `sqlite_orch_run.rs` 所有 `SELECT ... FROM orch_run_tasks`（`FromRow` 依赖列存在）补 `on_fail`。grep `output_files` 在该文件的 SELECT 列表定位所有 task 查询点，逐一加 `on_fail`。

- [ ] **Step 4: 写失败测试**（`engine.rs` mod tests）

需要一个「一节点失败 + 一独立节点成功」的图，失败节点 `on_fail="skip_and_continue"`：
```rust
    #[tokio::test]
    async fn skip_and_continue_failed_node_completes_with_failures() {
        // 两个独立节点:A 永久失败(on_fail=skip_and_continue)、B 成功。
        // A 无下游 → 无可跳过者;run 全部 settled、无硬失败、有 skip_and_continue 失败
        // → completed_with_failures,B 的产物照常交付。
        let (svc, engine, run_id) = two_independent_nodes_harness(
            /* A worker */ Arc::new(AlwaysFailWorker),
            /* A on_fail */ "skip_and_continue",
        ).await;
        engine.start(run_id.clone());
        let detail = drive_to_terminal(&svc, &run_id).await;
        assert_eq!(detail.run.status, "completed_with_failures");
        let a = detail.tasks.iter().find(|t| t.title.contains("A")).unwrap();
        let b = detail.tasks.iter().find(|t| t.title.contains("B")).unwrap();
        assert_eq!(a.status, "failed");
        assert_eq!(b.status, "done", "independent branch still delivers");
    }

    #[tokio::test]
    async fn fail_run_policy_fails_whole_run() {
        // 默认策略:A 失败(on_fail=NULL/fail_run) → 整 run failed(现状保持)。
        let (svc, engine, run_id) = two_independent_nodes_harness(
            Arc::new(AlwaysFailWorker), "fail_run",
        ).await;
        engine.start(run_id.clone());
        let detail = drive_to_terminal(&svc, &run_id).await;
        assert_eq!(detail.run.status, "failed");
    }
```
> `two_independent_nodes_harness` / `drive_to_terminal`：若不存在，基于既有 `ChainPlanProducer`/`SingleTaskPlanProducer` + `drive_to_completion` 新建 helper（构造两节点 DAG，给 A 落 `on_fail`）。`drive_to_terminal` = `wait_run_status` 直到 status ∈ {completed, completed_with_failures, failed, cancelled}。实现 Step 时按实际 helper 命名对齐；`AlwaysFailWorker` 现覆写 `last_error_present=true`（Task 1 Step 5）→立即失败无重试。

- [ ] **Step 5: 引擎——永久失败按策略跳下游 + 终态分流**

① `mark_task_failed` 之后按策略跳下游：在 `settle_failed_or_retry` 的 `else` 分支（调 `mark_task_failed` 后），或在 `mark_task_failed` 内末尾，读该 task 的 `on_fail`；若 `== Some("skip_and_continue")`，取 `dep_edges = deps.run_repo.list_deps(run_id)` 后 `skip_downstream(deps, run_id, task_id, &dep_edges).await`。（放在 `settle_failed_or_retry` else 分支更清晰——`mark_task_failed` 保持单一职责。）示例（`settle_failed_or_retry` else 分支）：
```rust
    } else {
        mark_task_failed(deps, run_id, task_id, conv_from_outcome, tokens).await;
        // 部分交付:skip_and_continue 策略下,跳过该失败节点的传递性下游,
        // 让独立分支跑完、run 终态走 completed_with_failures。
        if let Ok(Some(t)) = deps.run_repo.get_task(task_id).await {
            if t.on_fail.as_deref() == Some("skip_and_continue") {
                if let Ok(edges) = deps.run_repo.list_deps(run_id).await {
                    skip_downstream(deps, run_id, task_id, &edges).await;
                }
            }
        }
    }
```

② 终态判定（`engine.rs:1201-1279`）——把 `all_terminal`/`any_failed` 逻辑改为按策略分流。替换 `1221-1241` 区块为：
```rust
                        Ok(tasks) => {
                            // 已 settled = 每个任务处于终态(done/skipped)或「失败但策略是
                            // skip_and_continue」。硬失败 = 失败且策略为 fail_run(默认)。
                            let is_soft_fail = |t: &OrchRunTaskRow| {
                                t.status == "failed" && t.on_fail.as_deref() == Some("skip_and_continue")
                            };
                            let all_settled = tasks.iter().all(|t| {
                                t.status == "done" || t.status == "skipped" || is_soft_fail(t)
                            });
                            let hard_failed = tasks.iter().any(|t| {
                                t.status == "failed" && !is_soft_fail(t)
                            });
                            let any_soft_fail = tasks.iter().any(is_soft_fail);

                            if hard_failed {
                                let total_tokens = sum_task_tokens(&tasks);
                                finish_run(&deps, run_id, "failed", None, total_tokens).await;
                                handles.remove_if(run_id, |_, h| h.generation == generation);
                                TerminalDecision::FinishedFailed { tasks }
                            } else if !tasks.is_empty() && all_settled {
                                let total_tokens = sum_task_tokens(&tasks);
                                TerminalDecision::Completed { tasks, total_tokens }
                                // (any_soft_fail 决定最终写 completed vs completed_with_failures,
                                //  在 Completed 处理分支里判定 —— 见 ③)
                            } else {
                                let now = now_ms();
                                let awaiting_retry = tasks.iter().any(|t| {
                                    t.status == "pending" && t.next_retry_at.is_some_and(|at| at > now)
                                });
                                if awaiting_retry { TerminalDecision::IdleRetry }
                                else {
                                    warn!(run_id, task_count = tasks.len(),
                                        "Run loop: no ready tasks and run not terminal — exiting to avoid spin");
                                    TerminalDecision::Break
                                }
                            }
                        }
```
> `any_soft_fail` 需带到 Completed 处理分支。最简：给 `TerminalDecision::Completed` 加一个 `with_failures: bool` 字段（`Completed { tasks, total_tokens, with_failures }`），在此处 `with_failures: any_soft_fail`。

③ Completed 处理分支（`engine.rs:1281-1373` 内、`finish_run(..., "completed", ...)` 处 ~1358）：按 `with_failures` 选终态字符串：
```rust
    let terminal_status = if with_failures { "completed_with_failures" } else { "completed" };
    finish_run(&deps, run_id, terminal_status, Some(summary.clone()), total_tokens).await;
```
并把 `terminal_report` 的 outcome 对应设置（`with_failures` → 仍用 `RunOutcome::Completed`，摘要里注明有失败节点；Task 3 会加 NodeFailed 用于中途，这里终态保持 Completed 语义即可）。

- [ ] **Step 6: run_service re-arm 终态门 + FE 状态**

`run_service.rs:834`、`:965`、`:1716` 三处 `matches!(current_status.as_str(), "completed" | "failed" | "cancelled")` 各加 `| "completed_with_failures"`（rerun/adjust 能重新激活一个部分完成的 run）。
FE：`runStatusMeta.ts` 加 `completed_with_failures: { color: 'var(--warning)', key: 'completed_with_failures' }`；搜索该文件注释提到的页面 `STATUS_META` + `RunHistory` 并行 map 一并加；i18n `orchestrator.json` run `status` 块（en/zh `:136`）加 `"completed_with_failures": "Completed with failures"` / `"部分完成"`；仓库根 `bun run gen:i18n`；`ui/` `bun run typecheck` exit 0。

- [ ] **Step 7: 跑测试**

Run: `cargo nextest run -p nomifun-orchestrator ; cargo nextest run -p nomifun-db`
Expected: PASS（新增两测试绿；既有终态/完成/失败测试不回归——注意既有 `completed_run_reports_once_to_lead` 等仍写 `"completed"`，因无 soft-fail 节点）。
Run（`ui/`）：`bun run typecheck` → exit 0。

- [ ] **Step 8: 提交**
```bash
git add crates/backend/nomifun-db crates/backend/nomifun-orchestrator ui/src/renderer/pages/orchestrator ui/src/renderer/services/i18n/locales
git commit -m "feat(orch): 节点 on_fail 策略 + completed_with_failures 部分交付"
```

---

### Task 3: 节点永久失败中途上报 lead 修复

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/engine.rs`（`RunOutcome` 加 `NodeFailed`；`settle_failed_or_retry` else 分支中途回执）
- Test: `engine.rs` mod tests

**Interfaces:**
- Produces: `RunOutcome::NodeFailed`（`as_str() == "node_failed"`）；永久失败时 best-effort 调 `deps.lead_reporter.report(lead_conv_id, run_id, RunOutcome::NodeFailed, &brief)`，`brief` 含失败节点标题 + `last_error`。
- Consumes: 既有 `LeadReporter`、`get_run`（取 `lead_conv_id`）、`get_task`（取 `last_error`/`title`）。
- **约束**：绝不在 `RunLocks` 锁内 await（本调用在 `settle_failed_or_retry`，不持终态锁）；失败仅 `warn!`；不改终态语义（终态回执仍在 loop 退出后发一次）。中途回执**每永久失败节点一次**（天然有界）。

- [ ] **Step 1: 写失败测试**（`engine.rs` mod tests，复用 `RecordingLeadReporter` + `reporter_stack`）
```rust
    #[tokio::test]
    async fn permanent_node_failure_reports_to_lead_midrun() {
        // AlvaysFail 节点(带标记非重试)永久失败 → lead 会话在 run 到终态前
        // 收到一条 node_failed 中途回执(含失败原因)。
        let worker: Arc<dyn WorkerRunner> = Arc::new(AlwaysFailWorker);
        let reporter = Arc::new(RecordingLeadReporter::default());
        let (svc, engine, run_id) = lead_run_harness(worker, reporter.clone(), 0).await; // max_retries 0
        engine.start(run_id.clone());
        let _ = drive_to_terminal(&svc, &run_id).await;
        let reports = reporter.reports.lock().unwrap().clone();
        assert!(reports.iter().any(|(_, _, o)| o == "node_failed"),
            "a permanent node failure must produce a mid-run node_failed lead report");
    }
```
> `lead_run_harness`：复用既有 `reporter_stack(worker, max_retries)` + `adhoc_lead_run(run_service, lead_conv_id)` 组合（见 `engine.rs:3915/3945`）返回 `(svc, engine, run_id)` 且 run 绑定 `lead_conv_id`。按实际 helper 命名对齐。

- [ ] **Step 2: 运行确认失败**

Run: `cargo nextest run -p nomifun-orchestrator permanent_node_failure_reports_to_lead_midrun`
Expected: FAIL（无 `node_failed` 回执 / `RunOutcome::NodeFailed` 未定义）。

- [ ] **Step 3: 加 `RunOutcome::NodeFailed`**（`engine.rs:120-144`）
```rust
pub enum RunOutcome {
    Completed,
    Failed,
    Stalled,
    AwaitingApproval,
    NodeFailed,
}
```
`as_str` 加 `RunOutcome::NodeFailed => "node_failed",`。

- [ ] **Step 4: 中途回执**（`settle_failed_or_retry` else 分支，`mark_task_failed`/skip 之后）
```rust
        // 中途上报 lead:某必需/软失败节点永久失败时,best-effort 唤起 master 会话让其
        // 决策(改图/换模型重跑/放弃),不等终态。绝不在终态锁内;失败仅 warn。
        if let Ok(Some(run)) = deps.run_repo.get_run(run_id).await {
            if let Some(lead_conv_id) = run.lead_conv_id {
                let brief = match deps.run_repo.get_task(task_id).await {
                    Ok(Some(t)) => format!(
                        "节点「{}」永久失败：{}",
                        t.title,
                        t.last_error.as_deref().unwrap_or("未知原因")
                    ),
                    _ => format!("run {run_id} 的一个节点永久失败"),
                };
                if let Err(e) = deps
                    .lead_reporter
                    .report(lead_conv_id, run_id, RunOutcome::NodeFailed, &brief)
                    .await
                {
                    warn!(run_id, task_id, error = %e, "mid-run node-failed lead report failed");
                }
            }
        }
```
> 放在 else 分支「mark_task_failed（+可能的 skip_downstream）之后」，确保 `last_error` 已持久化可读。

- [ ] **Step 5: 跑测试（含既有 lead 测试不回归）**

Run: `cargo nextest run -p nomifun-orchestrator`
Expected: PASS。**注意**：`failed_run_reports_once_to_lead` 现会**额外**收到一条中途 `node_failed`（在终态 `failed` 之前）——该测试若断言「exactly one report」会回归。Step 实现时核对该测试：若它断言唯一回执，改为断言「恰有一条 `failed` 终态回执」而非「总回执数=1」（中途 `node_failed` 是新增的正确行为）。更新该断言并在报告中说明。

- [ ] **Step 6: 提交**
```bash
git add crates/backend/nomifun-orchestrator/src/engine.rs
git commit -m "feat(orch): 节点永久失败中途上报 lead(node_failed)"
```

---

### Task 4: 规划器收尾验收门（复杂 run）

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/plan.rs`（`PLAN_SYSTEM` 追加收尾验收规则）
- Test: `plan.rs` mod tests

**Interfaces:**
- Produces: `PLAN_SYSTEM` 内新增一条指令——对多阶段/正确性关键的复杂目标，规划器在汇聚处加一个收尾 `verify` 门（复用既有 verify+skeptics 语义）校验最终产物达标后再收尾。
- Consumes: 既有 verify 聚合器 `settle_verify_task`（无需引擎改动，planner 产出即可）。
- **本任务为提示级**（低风险、非确定性）：不做后处理强插。确定性强插留作后续硬化（Phase 2）。

- [ ] **Step 1: 写测试**（`plan.rs` mod tests，仿 `plan_system_teaches_verify_pattern`）
```rust
    #[test]
    fn plan_system_teaches_closing_acceptance_gate() {
        assert!(super::PLAN_SYSTEM.contains("验收") || super::PLAN_SYSTEM.contains("acceptance"),
            "PLAN_SYSTEM must instruct a closing acceptance/verify gate for complex goals");
        // 与既有 verify 教学并存
        assert!(super::PLAN_SYSTEM.contains("verify"));
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo nextest run -p nomifun-orchestrator plan_system_teaches_closing_acceptance_gate`
Expected: FAIL（`PLAN_SYSTEM` 尚无验收门指令）。

- [ ] **Step 3: 追加 `PLAN_SYSTEM` 规则**（`plan.rs`，在 verify 教学段之后追加一句，保持 JSON 转义风格一致）
在 `PLAN_SYSTEM` 常量的 verify 组合规则段（~`plan.rs:580`）之后追加（注意该常量是带 `\n\` 续行的转义字符串，按同款风格）：
```
  - 收尾验收(acceptance)：当目标是多阶段/正确性关键(会有他人基于产物继续工作、或交付物必须达标)时，在最终汇聚处加一个收尾 verify 门——用 2-3 个独立 skeptic agent 对照原始目标校验「最终产物是否达标、是否遗漏关键交付」，再由一个 kind:\"verify\" 任务按 majority 汇总；只有验收 PASS 才算真正完工。简单/单步目标不必加，避免过度工程。\n\
```

- [ ] **Step 4: 跑测试**

Run: `cargo nextest run -p nomifun-orchestrator plan`
Expected: PASS（新测试绿；既有 `plan_system_teaches_verify_pattern`、`parse_plan_*` 不回归）。

- [ ] **Step 5: 提交**
```bash
git add crates/backend/nomifun-orchestrator/src/plan.rs
git commit -m "feat(orch): 规划器收尾验收门(复杂 run)"
```

---

## 自审（Self-Review）

**1. Spec 覆盖（对照 spec §4）：**
- §4.2① 重试分类修正 + run 级预算 → Task 1（超时/空回复经 NoMarker 三态分类有界重试 + 无状态 run 级总预算）。✅
- §4.2② 失败→中途上报 lead 修复 → Task 3（`RunOutcome::NodeFailed` 中途回执）。✅
- §4.2③ 部分交付（`on_fail` + `completed_with_failures`）→ Task 2。✅
- §4.2④ 完成质量门 → Task 4（提示级收尾验收门，复用 verify）。⚠️ 提示级非确定性——确定性后处理强插显式留 Phase 2（Task 4 说明）。
- §4.2⑤ 产物登记（`output_files`）→ **归入 Phase 1b（W5）**（与共享工作目录/文件交接一起做），本 1a 不含。符合分期。

**2. Placeholder 扫描：** 无 TBD/TODO。测试 helper（`single_task_harness`/`two_independent_nodes_harness`/`drive_to_terminal`/`lead_run_harness`）标注为「若不存在则基于既有 X 新建，按实际命名对齐」——是具体的复用指令 + 回退，非空泛占位（实现者见既有 `retry_harness`/`reporter_stack`/`drive_to_completion` 即可派生）。

**3. 类型一致性：** `on_fail: Option<String>` 在迁移/`OrchRunTaskRow`/引擎读取处一致取值 `"fail_run"|"skip_and_continue"|NULL`。`RunOutcome::NodeFailed`/`as_str "node_failed"` 一致。`completed_with_failures` 字符串在引擎终态写、run_service 3 处 re-arm 门、FE runStatusMeta/i18n 一致。`max_timeout_retries`/`DEFAULT_MAX_TIMEOUT_RETRIES` 命名一致。`TerminalDecision::Completed { tasks, total_tokens, with_failures }` 加字段后所有构造/匹配点同步（Step 5②③ 与 1281-1373 处理分支）。

**4. 回归风险已点名（关键）：** Task 1 Step 5 与 Task 3 Step 5 明确指出既有测试（`AlwaysFailWorker` 的 NoMarker 归类、`failed_run_reports_once_to_lead` 的回执计数）需要的配套调整（给 `AlwaysFailWorker` 覆写 `last_error_present=true`；把「唯一回执」断言改为「唯一终态回执」）——这些是实现时必须处理的既有测试交互，已在计划内显式化，非事后惊喜。

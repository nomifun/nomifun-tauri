# Phase 1b — 节点间共享上下文 (W5) 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让一个编排 run 的各节点共享上下文，消除「节点各自为战、执行偏差」——自动给每个 run 分配一个共享工作目录、在其中维护人类可读的共享笔记 `RUN_NOTES.md`（run 目标 + 计划），并把「目标 + 计划摘要 + 传递性祖先产出摘要 + 共享笔记指针」注入每个下游节点的 brief（现状只注入直接上游、且不截断）。

**Architecture:** 全部在 `nomifun-orchestrator` 引擎内，零迁移。当前 ad-hoc/会话原生 run（`workspace_id=None` 且无 `work_dir`）的每个节点各自落到独立临时目录 → 无法共享文件；根因在 `run_loop` 的 `workspace_dir` 解析（两分支都 None）。方案：给 `RunEngineDeps` 线程一个 base 数据目录（`services.work_dir`），`run_loop` 在无目录时确定性分配 `<base>/orchestrator/runs/{run_id}` 并持久化到 `run.work_dir`（跨重启稳定、可 browse、且是绝对路径→触发共享 workpath KB 绑定）。所有节点经既有 `extra.workspace` 拿到同一 cwd，内置 Read/Write/nomi_fs_* 直接读写共享目录 + `RUN_NOTES.md`。brief 注入在 `dispatch_task`（有 deps/run_id）收集后传入纯函数 `compose_brief`。

**Tech Stack:** Rust（`nomifun-orchestrator`、`nomifun-db` 仅加 `UpdateRunParams.work_dir`、`nomifun-app` 接线 base dir），cargo-nextest。无 FE。

## Global Constraints

- **Rust 测试**：`cargo nextest run -p nomifun-orchestrator`；改 `UpdateRunParams` 后另跑 `cargo nextest run -p nomifun-db`；**跨 crate 编译校验必跑 `cargo check -p nomifun-app`**（Phase 1a 教训：`-p 单crate` 会漏下游消费者/构造点）。无 `| tail`；`cargo check` 不编译 test。
- **零迁移**：本相不加迁移（`orch_workspaces.context` / `orch_run_tasks.output_files` 列虽存在但**本相不用**——见「范围收窄」）。
- **不变量勿破**：`run_loop` 的 `workspace_dir` 现在只解析一次；持久化 `work_dir` 用 `update_run`（非终态锁内、best-effort，失败仅 warn 不影响执行）。boot-resume 时 `resolve_run_dir`/`run_loop` 读回已持久化的 `work_dir` → 稳定。
- **共享目录必须是绝对真实路径**，**不得**落在 `workspace_root/conversations/*-temp-*` 下——否则 `session_workpath_key` 归并为 `__default__` 哨兵，KB 绑定退化、且与 per-conversation 临时目录混淆。用 `<base>/orchestrator/runs/{run_id}`。
- **文件 I/O best-effort**：写 `RUN_NOTES.md` / `create_dir_all` 失败只 warn，绝不失败 run（磁盘满/权限问题不该炸编排）。
- **注入截断**：所有注入上游/祖先文本走 `truncate_summary_output`（`engine.rs:2054`，`SUMMARY_TASK_OUTPUT_LEN=600`，CJK 安全）——现状直接上游注入无上限，须补。
- **Git**：分支 `feat/phase1-reliability-shared-context`（续用，Phase 1a 已在其上）。逐任务提交。提交前 `git pull --rebase`。

## 范围收窄（本相有意不做，记录以免误判为缺口）

- **DB 列黑板 `orch_workspaces.context`**：该列只存在于 workspace-backed run；ad-hoc/会话原生 run（常见）`workspace_id=None` 无此行 → 对常见路径无效。故本相**不复活该列**；`RUN_NOTES.md`（共享目录内文件）作为**通用**共享记忆载体，覆盖所有 run。DB 列留待未来 workspace 级特性。
- **`orch_run_tasks.output_files` 每节点产物登记**：共享目录下多节点并发写，"哪个文件是本节点产出"无干净信号（扫目录与并行写竞态；`WorkerOutcome` 不带文件清单）。需要 per-node 文件 manifest 机制，本相**延后**。节点间文件共享经共享目录已可行（下游直接读上游写的文件）。
- **专用 `shared_notes` 网关工具**：W5 勘察确认——因 `extra.workspace`=共享目录，内置 Read/Write/nomi_fs_* 已能读写 `RUN_NOTES.md`，专用工具唯一增益是「固定路径约定」。本相**不加**专用工具，靠 brief 指针告知节点可读写 `RUN_NOTES.md`。

---

### Task 1: 自动分配并持久化 run 共享工作目录

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/engine.rs`（`RunEngineDeps` 加 `data_dir: PathBuf` + `::new` 默认；`run_loop` 的 `workspace_dir` 解析块 897-918 在两分支皆 None 时自动分配 + `create_dir_all` + 持久化 `run.work_dir`）
- Modify: `crates/backend/nomifun-db/src/repository/orch_run.rs`（`UpdateRunParams` 加 `work_dir: Option<Option<String>>`）
- Modify: `crates/backend/nomifun-db/src/repository/sqlite_orch_run.rs`（`update_run` 绑定 `work_dir`）
- Modify: `crates/backend/nomifun-app/src/router/state.rs`（构造 `RunEngineDeps` 处传入 `services.work_dir`）
- Test: `engine.rs` mod tests

**Interfaces:**
- Produces: `RunEngineDeps.data_dir: std::path::PathBuf`（编排数据根，run 共享目录的父）；`UpdateRunParams.work_dir: Option<Option<String>>`（`None`=不改，`Some(Some(p))`=设，`Some(None)`=清）。
- Consumes: 既有 `run.work_dir`/`run.workspace_id`、`ws_repo.get`、`update_run`、`resolve_run_dir`（自动受益）。
- 分配规则：`data_dir.join("orchestrator").join("runs").join(run_id)`（绝对路径）。持久化后 `resolve_run_dir`（browse）与后续 loop 重启都读回它。

- [ ] **Step 1: 写失败测试**（`engine.rs` mod tests）

复用/扩展 `diamond_harness`（该 harness 现给 workspace 一个 `workspace_dir`；本测试要一个**无 work_dir、无 workspace 的 ad-hoc run**）。新增 helper `adhoc_no_dir_harness(worker)`（create_adhoc with `work_dir=None`，无 workspace），并给 `RunEngineDeps` 设一个临时 `data_dir`（用 `std::env::temp_dir().join(format!("nomifun-test-{run_id}"))` 或测试专用临时目录）。测试：
```rust
    #[tokio::test]
    async fn adhoc_run_auto_allocates_shared_workspace_dir() {
        // 一个无 work_dir / 无 workspace 的 ad-hoc run:引擎应自动分配一个
        // <data_dir>/orchestrator/runs/{run_id} 共享目录,持久化到 run.work_dir,
        // 并把它作为 workspace_dir 传给每个节点(此前每节点各自临时目录、无法共享)。
        let worker = Arc::new(ConcurrencyMockWorkerRunner::new(Duration::from_millis(10)));
        let (svc, engine, run_id, data_dir) = adhoc_no_dir_harness(worker.clone()).await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        // 持久化了 work_dir
        let expected = data_dir.join("orchestrator").join("runs").join(&run_id);
        assert_eq!(detail.run.work_dir.as_deref(), Some(expected.to_str().unwrap()),
            "auto-allocated shared dir must be persisted to run.work_dir");
        assert!(expected.is_dir(), "shared dir must be created on disk");
        // 每个节点都拿到同一个共享目录
        let dirs = worker.seen_workspace_dir.lock().unwrap().clone();
        assert!(!dirs.is_empty());
        for d in &dirs {
            assert_eq!(d.as_deref(), expected.to_str(), "every node shares the one run dir");
        }
    }
```
> `ConcurrencyMockWorkerRunner` 已捕获 `seen_workspace_dir`（engine.rs:5180-5234）。`adhoc_no_dir_harness` 若不存在则基于既有 adhoc 构造（`create_adhoc` req `work_dir=None`）+ 一个多节点 plan producer 派生，返回 `(svc, engine, run_id, data_dir)`；`data_dir` 是给 `RunEngineDeps.data_dir` 的临时目录。按实际 helper 命名对齐。

- [ ] **Step 2: 运行确认失败**

Run: `cargo nextest run -p nomifun-orchestrator adhoc_run_auto_allocates_shared_workspace_dir`
Expected: FAIL（现状 workspace_dir=None，每节点独立临时目录，`run.work_dir` 仍 None）。

- [ ] **Step 3: `RunEngineDeps` 加 `data_dir` + 接线**

`engine.rs` `RunEngineDeps`（字段区）：
```rust
    /// Base data dir under which a workspace-less (ad-hoc) run gets an
    /// auto-allocated shared working directory (`<data_dir>/orchestrator/runs/{id}`),
    /// so all its nodes share one cwd (files + workpath KB). See `run_loop`.
    pub data_dir: std::path::PathBuf,
```
`RunEngineDeps::new`（默认）：`data_dir: std::path::PathBuf::from("."),`（测试可覆写；生产由 app 注入真实 dir）。
`nomifun-app/src/router/state.rs`（构造 RunEngineDeps 处——grep `RunEngineDeps::new` 或 `RunEngineDeps {`）：设 `data_dir: services.work_dir.clone()`（`services.work_dir` 在此可得，见 state.rs:1103/1193）。若用 struct-literal 构造则加该字段；若用 `::new` 后 setter 则加一行赋值。

- [ ] **Step 4: `UpdateRunParams.work_dir` + SQL**

`orch_run.rs` `UpdateRunParams`：加 `pub work_dir: Option<Option<String>>,`（放在既有字段旁；所有构造点补 `work_dir: None`——grep `UpdateRunParams {` 全项目补，含 `finish_run`/`transition`/其它）。
`sqlite_orch_run.rs` `update_run`：仿既有可选列（如 `summary`）的条件绑定，加 `if let Some(wd) = &p.work_dir { sets.push("work_dir = ?"); ... bind }`（`Some(None)`→NULL，`Some(Some(v))`→v）。

- [ ] **Step 5: `run_loop` 自动分配**

`engine.rs` 的 `workspace_dir` 解析块（897-918），在两分支皆得 None 后追加：
```rust
    // Ad-hoc/conversation-native run with no dir: auto-allocate a per-run SHARED
    // directory so all nodes share one cwd (files + a shared workpath KB binding).
    // Deterministic path (stable across restarts); persisted to run.work_dir so
    // browse + boot-resume resolve it. best-effort — a create/persist failure logs
    // and falls back to per-node temp dirs (no run failure).
    let workspace_dir: Option<String> = match workspace_dir {
        Some(d) => Some(d),
        None => {
            let dir = deps.data_dir.join("orchestrator").join("runs").join(run_id);
            match std::fs::create_dir_all(&dir) {
                Ok(()) => {
                    let dir_str = dir.to_string_lossy().to_string();
                    if let Err(e) = deps.run_repo.update_run(run_id, UpdateRunParams {
                        work_dir: Some(Some(dir_str.clone())),
                        ..Default::default()
                    }).await {
                        warn!(run_id, error=%e, "failed to persist auto shared work_dir (continuing)");
                    }
                    Some(dir_str)
                }
                Err(e) => { warn!(run_id, error=%e, "failed to create shared run dir (nodes use temp dirs)"); None }
            }
        }
    };
```
> 需确认 `UpdateRunParams: Default`（Phase 1a 的 `UpdateTaskParams` 有 Default；`UpdateRunParams` 若无 Default 则显式全 `None` 构造）。放在解析块之后、进入 fill 循环之前（每 run 一次）。

- [ ] **Step 6: 跑测试 + 跨 crate 校验**

Run: `cargo nextest run -p nomifun-orchestrator` — 新测试绿 + 全绿。
Run: `cargo nextest run -p nomifun-db` — `UpdateRunParams` 改动不回归。
Run: `cargo check -p nomifun-app` — 接线 + `UpdateRunParams` 全构造点补 `work_dir: None` 后编译干净。
No `| tail`.

- [ ] **Step 7: 提交**
```bash
git add crates/backend/nomifun-orchestrator crates/backend/nomifun-db crates/backend/nomifun-app
git commit -m "feat(orch): 自动分配并持久化 run 共享工作目录"
```

---

### Task 2: 共享笔记 `RUN_NOTES.md`（run 目标 + 计划摘要）

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/engine.rs`（run 启动时写 `RUN_NOTES.md`）
- Test: `engine.rs` mod tests

**Interfaces:**
- Produces: 共享目录内 `RUN_NOTES.md`，含 run 目标 + 计划（各节点 title），并留「## 共享笔记（各节点可追加发现/结论）」区。节点经 cwd 内置文件工具可读/追加。
- Consumes: Task 1 的 `workspace_dir`（共享目录）、`run.goal`、`list_tasks`（节点 title）。
- 幂等：run 启动写一次；若已存在（重启/重跑）不覆盖已追加内容——**只在文件不存在时创建**（`if !path.exists()`），保住节点追加的内容。

- [ ] **Step 1: 写失败测试**
```rust
    #[tokio::test]
    async fn run_start_seeds_run_notes_md() {
        let worker = Arc::new(ConcurrencyMockWorkerRunner::new(Duration::from_millis(10)));
        let (svc, engine, run_id, data_dir) = adhoc_no_dir_harness(worker.clone()).await;
        engine.start(run_id.clone());
        let _ = drive_to_completion(&svc, &run_id).await;
        let notes = data_dir.join("orchestrator").join("runs").join(&run_id).join("RUN_NOTES.md");
        assert!(notes.is_file(), "RUN_NOTES.md seeded in the shared dir");
        let body = std::fs::read_to_string(&notes).unwrap();
        assert!(body.contains("目标") || body.contains("GOAL"), "notes carry the run goal");
        // 计划摘要含节点标题(harness 的 plan 至少含一个已知 title)
        assert!(body.contains("共享笔记"), "has an append region for node findings");
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo nextest run -p nomifun-orchestrator run_start_seeds_run_notes_md`
Expected: FAIL（无 RUN_NOTES.md）。

- [ ] **Step 3: 实现 seed**

在 `run_loop`，Task 1 分配 `workspace_dir` 之后（run 首次进入、fill 循环前），加一个 best-effort helper 调用：
```rust
    if let Some(ref dir) = workspace_dir {
        seed_run_notes(&deps, run_id, dir).await;
    }
```
新增自由 fn（`engine.rs`）：
```rust
/// Seed a human-readable shared notes file in the run's shared dir (best-effort,
/// idempotent — only if absent, to preserve node-appended content across restart).
/// Carries the run goal + plan (task titles) + an append region nodes can add
/// findings to via built-in file tools.
async fn seed_run_notes(deps: &Arc<RunEngineDeps>, run_id: &str, dir: &str) {
    let path = std::path::Path::new(dir).join("RUN_NOTES.md");
    if path.exists() {
        return;
    }
    let goal = deps.run_repo.get_run(run_id).await.ok().flatten()
        .map(|r| r.goal).unwrap_or_default();
    let tasks = deps.run_repo.list_tasks(run_id).await.unwrap_or_default();
    let mut body = String::from("# RUN_NOTES\n\n## 目标 (GOAL)\n");
    body.push_str(&goal);
    body.push_str("\n\n## 计划 (PLAN)\n");
    for t in &tasks {
        body.push_str(&format!("- {}\n", t.title));
    }
    body.push_str("\n## 共享笔记（各节点可追加发现/结论，供其它节点参考）\n");
    if let Err(e) = std::fs::write(&path, body) {
        warn!(run_id, error=%e, "failed to seed RUN_NOTES.md (continuing)");
    }
}
```
> 若 `list_tasks` 此刻可能为空（计划尚未持久化），确认 seed 调用点在计划已持久化之后（run_loop 首个 fill 前，tasks 已在库）。

- [ ] **Step 4: 跑测试**

Run: `cargo nextest run -p nomifun-orchestrator` — 新测试绿 + 全绿。

- [ ] **Step 5: 提交**
```bash
git add crates/backend/nomifun-orchestrator/src/engine.rs
git commit -m "feat(orch): run 启动 seed 共享笔记 RUN_NOTES.md(目标+计划)"
```

---

### Task 3: 下游 brief 注入增强（目标 + 计划摘要 + 传递性祖先 + 笔记指针 + 截断）

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/engine.rs`（`dispatch_task` 收集新上下文；`compose_brief`/`compose_agent_brief`/`compose_synthesis_brief` 加参数与新段；`collect_upstream_outputs` 旁加传递性祖先收集；直接上游注入补截断）
- Test: `engine.rs` mod tests

**Interfaces:**
- Produces: `compose_brief(role_hint, task, upstream, ctx: &BriefContext)`——新增 `BriefContext { goal: String, plan_summary: String, ancestor_digest: Vec<(String,String)>, notes_hint: Option<String> }`（或等价的独立参数）。brief 新段：`RUN GOAL`、`PLAN`（计划摘要）、`EARLIER CONTEXT (transitive ancestors)`（截断祖先摘要）、`SHARED NOTES: 可读/追加 <dir>/RUN_NOTES.md`。
- Consumes: `run.goal`、`list_tasks`（plan summary）、`list_deps` + BFS（传递性祖先，去重 `seen`，排除直接上游避免重复）、`truncate_summary_output`、Task 1 的 `workspace_dir`（notes 指针）。
- 截断：直接上游 + 祖先摘要都过 `truncate_summary_output`（现直接上游无截断）。

- [ ] **Step 1: 写失败测试**（扩展 brief 捕获 mock 或用 compose_* 纯函数测）

优先加纯函数测（快、稳）：
```rust
    #[test]
    fn compose_brief_includes_goal_plan_ancestors_and_notes_pointer() {
        let task = /* OrchRunTaskRow, kind="agent", title="Write", spec="..." */;
        let upstream = vec![("B".to_string(), "b out".to_string())];
        let ctx = BriefContext {
            goal: "构建报告".into(),
            plan_summary: "A → B → Write".into(),
            ancestor_digest: vec![("A".to_string(), "a findings".to_string())],
            notes_hint: Some("/ws/run1/RUN_NOTES.md".into()),
        };
        let brief = compose_brief(Some("writer"), &task, &upstream, &ctx);
        assert!(brief.contains("构建报告"), "run goal injected");
        assert!(brief.contains("A → B → Write"), "plan summary injected");
        assert!(brief.contains("a findings"), "transitive ancestor digest injected");
        assert!(brief.contains("b out"), "direct upstream still present");
        assert!(brief.contains("RUN_NOTES.md"), "shared-notes pointer injected");
    }

    #[test]
    fn compose_brief_truncates_long_upstream() {
        let long = "x".repeat(SUMMARY_TASK_OUTPUT_LEN + 500);
        let upstream = vec![("B".to_string(), long)];
        let ctx = BriefContext { goal: "g".into(), plan_summary: "p".into(),
            ancestor_digest: vec![], notes_hint: None };
        let brief = compose_brief(Some("w"), &task_min(), &upstream, &ctx);
        assert!(!brief.contains(&"x".repeat(SUMMARY_TASK_OUTPUT_LEN + 500)), "long upstream truncated");
        assert!(brief.contains('…'), "truncation marker present");
    }
```
> 既有 `compose_brief_includes_role_task_and_upstream`（engine.rs:4363）须同步更新为新签名（加 `&ctx` 参数，传一个空/默认 `BriefContext`），保持其原断言。

- [ ] **Step 2: 运行确认失败**

Run: `cargo nextest run -p nomifun-orchestrator compose_brief`
Expected: FAIL（`BriefContext`/新参数未定义；新段缺失）。

- [ ] **Step 3: 定义 `BriefContext` + 改 `compose_*` 签名与渲染**

`engine.rs`：
```rust
/// Cross-node context injected into every worker brief so a node sees the run
/// goal, the plan shape, relevant transitive-ancestor outputs (not just direct
/// deps), and where the shared notes live.
pub(crate) struct BriefContext {
    pub goal: String,
    pub plan_summary: String,
    /// (title, truncated output_summary) for transitive ancestors beyond direct deps.
    pub ancestor_digest: Vec<(String, String)>,
    /// Absolute path to RUN_NOTES.md (None when the run has no shared dir).
    pub notes_hint: Option<String>,
}
```
`compose_brief`/`compose_agent_brief`/`compose_synthesis_brief` 各加 `ctx: &BriefContext` 参数。在 `compose_agent_brief`（TASK/SPEC 之后、UPSTREAM RESULTS 之前）注入：
```rust
    if !ctx.goal.trim().is_empty() {
        out.push_str("\nRUN GOAL (整个编排的目标，你的产出要服务于它):\n");
        out.push_str(ctx.goal.trim());
        out.push('\n');
    }
    if !ctx.plan_summary.trim().is_empty() {
        out.push_str("\nPLAN (全局计划概要):\n");
        out.push_str(ctx.plan_summary.trim());
        out.push('\n');
    }
```
UPSTREAM RESULTS 每条改用 `truncate_summary_output(summary)`。其后加祖先段：
```rust
    if !ctx.ancestor_digest.is_empty() {
        out.push_str("\nEARLIER CONTEXT (更早的相关产出摘要):\n");
        for (title, summary) in &ctx.ancestor_digest {
            out.push_str(&format!("- {}: {}\n", title, summary)); // 已截断
        }
    }
    if let Some(notes) = &ctx.notes_hint {
        out.push_str(&format!("\nSHARED NOTES: 你可读取并追加 {notes}（记录发现/结论供其它节点参考）。\n"));
    }
```
`compose_synthesis_brief` 同理加 goal/plan/notes（祖先对 synthesis 意义小，可只加 goal + notes）。`compose_brief` 分发时把 `ctx` 透传两分支。

- [ ] **Step 4: `dispatch_task` 收集 `BriefContext`**

`dispatch_task`（engine.rs:1477-1479，`collect_upstream_outputs` 调用处）：
```rust
    let upstream = collect_upstream_outputs(deps, run_id, &task.id).await;
    let ctx = build_brief_context(deps, run_id, &task.id, workspace_dir.as_deref()).await;
    let brief = compose_brief(member.role_hint.as_deref(), &task, &upstream, &ctx);
```
新增 `build_brief_context`：读 `run.goal`；`plan_summary` = `list_tasks` 的 title 列（可含状态，短）；`ancestor_digest` = 传递性祖先（BFS 沿 `list_deps` 从本 task 向上收 blockers 的 blockers，去重、排除直接上游、每条 `truncate_summary_output`）；`notes_hint` = `workspace_dir.map(|d| format!("{d}/RUN_NOTES.md"))`（用 `Path::join` 规范）。祖先 BFS 镜像 `skip_downstream`（engine.rs:3488）的 `seen`-guarded 走法但方向相反（沿 blocked→blocker 向上）。

- [ ] **Step 5: 跑测试 + 跨 crate**

Run: `cargo nextest run -p nomifun-orchestrator` — 新测试 + 更新后的既有 compose_brief 测试全绿。
Run: `cargo check -p nomifun-app` — 若 `compose_brief`/`BriefContext` 是 pub 且被外部引用则确认编译（应仅 crate 内用）。
No `| tail`.

- [ ] **Step 6: 提交**
```bash
git add crates/backend/nomifun-orchestrator/src/engine.rs
git commit -m "feat(orch): 下游 brief 注入目标+计划+传递性祖先+笔记指针+截断"
```

---

## 自审（Self-Review）

**1. Spec 覆盖（对照 spec §5）：**
- §5.2① 共享工作目录 + 共享笔记文件 → Task 1（自动分配 + 持久化）+ Task 2（RUN_NOTES.md）。✅
- §5.2② run 黑板（结构化）→ **范围收窄**：DB 列不适配 ad-hoc run，改由 RUN_NOTES.md（通用文件）承担共享记忆；结构化黑板（DB 列/JSON 文件）延后。已在「范围收窄」记录。⚠️（对决策 C 的务实调整——RUN_NOTES.md 覆盖「共享 memory/知识文件」核心诉求）。
- §5.2③ 注入增强（修传递性丢失 + 截断）→ Task 3。✅
- §5.2④ output_files 产物登记 → **延后**（需 per-node manifest，共享目录并发写无干净归属信号）。已记录。
- KB workpath 交互：共享绝对目录 → `session_workpath_key` 稳定键 → 跨节点共享 KB 绑定（Global Constraints 明列约束：不得落 temp 目录）。✅

**2. Placeholder 扫描：** 无 TBD/TODO。测试 helper `adhoc_no_dir_harness` 标注「若不存在则基于既有 create_adhoc(work_dir=None) 派生，返回 (svc,engine,run_id,data_dir)」——具体复用指令 + 回退。`build_brief_context`/`seed_run_notes`/BFS 均给出完整代码或明确镜像对象（`skip_downstream` 的 seen-guarded 走法）。

**3. 类型一致性：** `RunEngineDeps.data_dir: PathBuf`（Task1 定义、state.rs 注入、run_loop 用）一致。`UpdateRunParams.work_dir: Option<Option<String>>`（Task1 定义 + 所有构造点补 None + SQL 绑定）一致。`BriefContext { goal, plan_summary, ancestor_digest, notes_hint }`（Task3 定义、compose_* 参数、build_brief_context 构造、测试构造）四处字段名一致。共享目录路径公式 `data_dir/orchestrator/runs/{run_id}` 在 run_loop 分配、Task1 测试断言、Task2/3 的 RUN_NOTES 路径一致。

**4. 跨 crate（Phase 1a 教训）：** Task 1 显式要求 `cargo check -p nomifun-app`（`UpdateRunParams` 加字段会破坏其所有 struct-literal 构造点，含 app/其它 crate；`RunEngineDeps` 加字段破坏其构造点）。已在 Global Constraints + Task1 Step6 强制。

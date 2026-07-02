# 子 Agent 可视化底座统一 (nomi_spawn 扁平编排) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 桌面会话（伙伴+普通）的并行子 agent 不再走静默的进程内 Spawn，而是走跳过 planner 的扁平编排 run（`nomi_spawn` 网关工具），复用编排 DAG 画布/worker 转录可视化；进程内 Spawn 仅留 CLI。

**Architecture:** 三层：① `nomi-tools`/`nomi-config`/`nomi-agent` 引擎层加两个 config 门控（`in_process_spawn` 注册门 + `builtin_allowlist` 工具白名单）；② `nomifun-orchestrator` 把 `plan()` 拆出可复用落库半段，新增 `plan_flat`（无 planner LLM）；③ `nomifun-gateway` 新增 `nomi_spawn` 能力（supervised 自主度、deny Remote），`nomifun-ai-agent` 工厂计算门控值。前端零改动（可视化纯数据驱动）。

**Tech Stack:** Rust (tokio/axum/sqlx/serde), workspace crates 见下。

## Global Constraints

- 提交作者：`git -c user.name=nomifun -c user.email=rika00@qq.com commit`（禁止 Claude 署名 / Co-Authored-By）。
- 禁 `cargo fmt`（全局）；提交信息中文。
- 编排不变式**绝不破坏**：per-run 锁不跨 LLM await；`link_orchestrator_run` 只 merge extra + 广播；侧栏过滤键 `orchestrator_task_id`；新网关能力必须 `.deny_on(ORCHESTRATOR_DENY_SURFACES)`。
- 所有新 config 字段默认值 = 现状行为（`in_process_spawn` 默认 `true`、`builtin_allowlist` 默认空 = 不限制）——CLI/既有会话零回归。
- 每个 Task 结束跑该 crate 测试；全部完成后跑 `cargo check --workspace` + 相关 crate 全量测试。
- 测试命令模式：`cargo test -p <crate> --lib <filter>`（在仓库根 `D:\code\nomifun\nomifun-tauri` 运行）。

---

### Task 1: `ToolRegistry::retain_named`（工具白名单的执行机制）

**Files:**
- Modify: `crates/agent/nomi-tools/src/registry.rs`

**Interfaces:**
- Produces: `pub fn retain_named(&mut self, allowed: &[String])` — 只保留名字在 `allowed` 里的已注册工具；`allowed` 为空时 no-op（不限制）。Task 3 的 bootstrap 调用它。

- [ ] **Step 1: 读现有 registry 结构**

Read `crates/agent/nomi-tools/src/registry.rs`（全文 ~67 行），确认内部存储（`HashMap`/`Vec`）与既有测试风格。

- [ ] **Step 2: 写失败测试**（追加到该文件已有 `#[cfg(test)] mod tests`；若无则新建，参考同 crate lib.rs 的测试风格）

```rust
#[test]
fn retain_named_keeps_only_allowed_and_empty_is_noop() {
    // 用两个最小假工具注册进 registry（参考本文件/lib.rs 已有测试的工具构造方式；
    // 若已有测试用 mock Tool，复用同一 mock）。
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(crate::glob::GlobTool::new(std::env::temp_dir())));
    registry.register(Box::new(crate::grep::GrepTool::new(std::env::temp_dir())));

    // 空 allowlist = 不限制。
    registry.retain_named(&[]);
    assert!(registry.get("Glob").is_some());
    assert!(registry.get("Grep").is_some());

    // 非空 = 只留白名单内的。
    registry.retain_named(&["Glob".to_string()]);
    assert!(registry.get("Glob").is_some());
    assert!(registry.get("Grep").is_none());
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test -p nomi-tools --lib retain_named`
Expected: FAIL（`retain_named` 未定义）

- [ ] **Step 4: 最小实现**（按实际内部存储写；若是 `HashMap<String, Box<dyn Tool>>`：）

```rust
/// Keep only the tools whose name is in `allowed`. An EMPTY `allowed` is a
/// no-op (= unrestricted), so callers can pass a config value straight through.
/// Used by restricted sub-agent worker sessions (per-node tool whitelist) —
/// registration-time filtering because the registry has no unregister.
pub fn retain_named(&mut self, allowed: &[String]) {
    if allowed.is_empty() {
        return;
    }
    self.tools.retain(|name, _| allowed.iter().any(|a| a == name));
}
```

（若字段名/结构不同，等价实现；不改其他方法。）

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test -p nomi-tools --lib`
Expected: 全部 PASS（含既有测试）

- [ ] **Step 6: Commit**

```bash
git add crates/agent/nomi-tools/src/registry.rs
git -c user.name=nomifun -c user.email=rika00@qq.com commit -m "feat(nomi-tools): ToolRegistry::retain_named 工具白名单（空=不限制）"
```

---

### Task 2: `ToolsConfig` 两个新字段

**Files:**
- Modify: `crates/agent/nomi-config/src/config.rs:181-259`（`ToolsConfig` struct + `Default` impl）

**Interfaces:**
- Produces: `ToolsConfig.in_process_spawn: bool`（默认 `true`）、`ToolsConfig.builtin_allowlist: Vec<String>`（默认空）。Task 3 的 bootstrap、Task 4 的 manager 消费。

- [ ] **Step 1: 写失败测试**（追加到 config.rs 已有测试模块；若该文件无测试模块则新建 `#[cfg(test)] mod tools_config_tests`）

```rust
#[test]
fn tools_config_new_fields_default_to_current_behavior() {
    let t = ToolsConfig::default();
    assert!(t.in_process_spawn, "默认必须保留进程内 Spawn（CLI 零回归）");
    assert!(t.builtin_allowlist.is_empty(), "默认不限制工具");
    // serde 缺字段也回落默认（旧 config 文件零回归）。
    let de: ToolsConfig = serde_json::from_str("{}").unwrap();
    assert!(de.in_process_spawn);
    assert!(de.builtin_allowlist.is_empty());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p nomi-config --lib tools_config_new_fields`
Expected: FAIL（字段不存在）

- [ ] **Step 3: 实现**——在 `ToolsConfig` struct（`cooperative_cancel` 字段之后）加：

```rust
    /// 是否注册进程内 `Spawn` 子 agent 工具（默认 true = 现状）。桌面后端会话
    /// 由工厂置 false —— 改走可视化的 `nomi_spawn` 编排扇出（子 agent 有 DAG
    /// 画布/转录）；CLI/独立模式保持 true（进程内 Spawn 仍是其唯一扇出）。
    #[serde(default = "default_true")]
    pub in_process_spawn: bool,
    /// 非空时：bootstrap 注册完全部工具后只保留名字在此列表内的（含 MCP 代理
    /// 工具）。受限角色的编排 worker（searcher/reviewer 只读等）用它做
    /// per-node 工具白名单。空（默认）= 不限制。
    #[serde(default)]
    pub builtin_allowlist: Vec<String>,
```

`Default` impl 加 `in_process_spawn: true, builtin_allowlist: Vec::new(),`。若文件里没有 `fn default_true() -> bool`，加：

```rust
fn default_true() -> bool {
    true
}
```

（先 grep `default_true` 确认是否已存在，避免重复定义。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p nomi-config --lib`
Expected: 全部 PASS

- [ ] **Step 5: Commit**

```bash
git add crates/agent/nomi-config/src/config.rs
git -c user.name=nomifun -c user.email=rika00@qq.com commit -m "feat(nomi-config): ToolsConfig 增 in_process_spawn 门控与 builtin_allowlist 白名单（默认=现状）"
```

---

### Task 3: bootstrap 应用两个门控

**Files:**
- Modify: `crates/agent/nomi-agent/src/bootstrap.rs`（SpawnTool 注册 ~:580-593；引擎构造前应用 retain）
- Modify: `crates/agent/nomi-agent/src/bootstrap_test.rs`（现有 :74 断言 Spawn 恒注册——改造）

**Interfaces:**
- Consumes: Task 1 `retain_named`、Task 2 两字段。
- Produces: `config.tools.in_process_spawn=false` 的引擎无 `Spawn` 工具；`builtin_allowlist` 非空的引擎只有白名单内工具。

- [ ] **Step 1: 改造/新增 bootstrap 测试**（打开 `bootstrap_test.rs`，找到 :74 附近断言 Spawn 注册的测试，保留其"默认注册"断言，新增两个用例；沿用该文件既有的 bootstrap 构造 helper）

```rust
#[tokio::test]
async fn spawn_tool_gated_off_when_in_process_spawn_false() {
    // 沿用本文件已有的 config/bootstrap 构造方式，仅翻转开关。
    let mut config = test_config(); // ← 用本文件实际的 helper 名
    config.tools.in_process_spawn = false;
    let (registry, _rest) = build_registry_via_bootstrap(config).await; // ← 用本文件实际的构建方式
    assert!(registry.get("Spawn").is_none(), "门控关闭时不得注册进程内 Spawn");
}

#[tokio::test]
async fn builtin_allowlist_restricts_registered_tools() {
    let mut config = test_config();
    config.tools.builtin_allowlist = vec!["Read".into(), "Grep".into(), "Glob".into()];
    let (registry, _rest) = build_registry_via_bootstrap(config).await;
    assert!(registry.get("Read").is_some());
    assert!(registry.get("Grep").is_some());
    assert!(registry.get("Glob").is_some());
    assert!(registry.get("Bash").is_none(), "白名单外的工具必须被过滤");
    assert!(registry.get("Write").is_none());
    assert!(registry.get("Spawn").is_none());
}
```

（实现者注意：`bootstrap_test.rs` 的实际 helper 名/构建方式以文件为准，上面两个用例的**断言**是需求；若 bootstrap 无法在测试中拿到 registry，则采用该文件 :74 现有测试同样的手段。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p nomi-agent --lib bootstrap`
Expected: 新测试 FAIL

- [ ] **Step 3: 实现门控**——`bootstrap.rs` :580-593 处，把 spawner 构造 + 注册整体包进条件：

```rust
        // 进程内 Spawn 门控（默认开）：桌面后端会话由工厂置 false —— 子 agent
        // 改走可视化的 nomi_spawn 编排扇出；CLI/独立模式保持进程内 Spawn。
        if self.config.tools.in_process_spawn {
            let spawner = Arc::new(
                crate::spawner::AgentSpawner::new(
                    provider.clone(),
                    self.config.clone(),
                    cwd_path.to_path_buf(),
                )
                .with_token_budget(
                    self.config
                        .tools
                        .subagent_token_budget
                        .map(|limit| Arc::new(crate::spawner::TokenBudget::new(limit))),
                ),
            );
            registry.register(Box::new(crate::spawn_tool::SpawnTool::new(spawner)));
        }
```

- [ ] **Step 4: 实现白名单**——在 bootstrap 里 registry **完成全部注册之后、被移交给引擎之前**（grep `AgentEngine::new` / registry 最后一次使用点，通常在 build() 尾部）插入：

```rust
        // Per-node 工具白名单（受限角色的编排 worker）：非空时只保留白名单内
        // 的工具（含 MCP 代理）。放在全部注册之后、引擎构造之前 —— registry
        // 没有 unregister，这是唯一收口点。空 = 不限制（默认，零回归）。
        registry.retain_named(&self.config.tools.builtin_allowlist);
```

- [ ] **Step 5: 跑测试确认通过 + 无回归**

Run: `cargo test -p nomi-agent --lib`
Expected: 全部 PASS（444+ 项；若某测试假设 Spawn 恒在，按 Step 1 改造其为"默认在"）

- [ ] **Step 6: Commit**

```bash
git add crates/agent/nomi-agent/src/bootstrap.rs crates/agent/nomi-agent/src/bootstrap_test.rs
git -c user.name=nomifun -c user.email=rika00@qq.com commit -m "feat(nomi-agent): bootstrap 接入 in_process_spawn 门控与 builtin_allowlist 白名单"
```

---

### Task 4: 后端工厂/manager 灌门控值

**Files:**
- Modify: `crates/backend/nomifun-api-types/src/agent_build_extra.rs`（`NomiBuildExtra` 加 `allowed_tools`）
- Modify: `crates/backend/nomifun-ai-agent/src/types.rs:61-144`（`NomiResolvedConfig` 加两字段）
- Modify: `crates/backend/nomifun-ai-agent/src/factory/nomi.rs`（计算 + 填充；~:377-433 `NomiResolvedConfig` 构造处）
- Modify: `crates/backend/nomifun-ai-agent/src/manager/nomi/agent.rs:236-261`（灌入 config.tools，紧邻 browser/computer）

**Interfaces:**
- Consumes: Task 2 的 `ToolsConfig` 字段。
- Produces: `NomiBuildExtra.allowed_tools: Vec<String>`（serde default 空；worker extra JSON 的 `allowed_tools` 键反序列化到此）；`NomiResolvedConfig.in_process_spawn: bool` + `allowed_tools: Vec<String>`；纯函数 `pub(crate) fn engine_spawn_enabled(desktop_gateway: bool, channel_platform: Option<&str>) -> bool`。

- [ ] **Step 1: 写纯函数失败测试**（加到 `factory/nomi.rs` 既有 `mod tests`，紧邻 `is_orchestration_lead_policy`）

```rust
#[test]
fn engine_spawn_enabled_policy() {
    // 本地桌面网关会话（普通/伙伴）→ 禁进程内 Spawn（改走 nomi_spawn 可视化扇出）。
    assert!(!engine_spawn_enabled(true, None));
    // IM 渠道 master 会话：nomi_spawn 对 Remote 面拒绝，保留进程内 Spawn。
    assert!(engine_spawn_enabled(true, Some("telegram")));
    // 无网关会话（不该出现在桌面，但语义上）→ 保留。
    assert!(engine_spawn_enabled(false, None));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p nomifun-ai-agent --lib engine_spawn_enabled`
Expected: FAIL（函数未定义）

- [ ] **Step 3: 实现**

(a) `agent_build_extra.rs` 的 `NomiBuildExtra`（`orchestrator_role` 字段后）加：

```rust
    /// Per-session 工具白名单（受限角色的编排 worker 用）。非空时引擎只保留
    /// 名单内的工具。后端（orchestrator worker）设置；普通会话恒空 = 不限制。
    #[serde(default)]
    pub allowed_tools: Vec<String>,
```

(b) `types.rs` 的 `NomiResolvedConfig`（`owner_token` 字段后）加：

```rust
    /// 是否注册进程内 Spawn（工厂按 engine_spawn_enabled 计算；CLI 恒 true）。
    pub in_process_spawn: bool,
    /// Per-session 工具白名单（空 = 不限制），源自 NomiBuildExtra.allowed_tools。
    pub allowed_tools: Vec<String>,
```

(c) `factory/nomi.rs`——在 `is_orchestration_lead` 函数旁加纯函数：

```rust
/// 进程内 Spawn 门控（纯函数，可单测）：本地桌面网关会话（desktop_gateway 且
/// 非 IM 渠道）禁用进程内 Spawn —— 子 agent 改走 nomi_spawn 编排扇出（可视化）；
/// IM 渠道 master（nomi_spawn 对 Remote 面拒绝）与其余会话保留进程内 Spawn。
pub(crate) fn engine_spawn_enabled(desktop_gateway: bool, channel_platform: Option<&str>) -> bool {
    !(desktop_gateway && channel_platform.is_none())
}
```

在 `NomiResolvedConfig { ... }` 构造处（~:390 `session_mode` 附近）加：

```rust
        in_process_spawn: engine_spawn_enabled(
            overrides.desktop_gateway,
            overrides.channel_platform.as_deref(),
        ),
        allowed_tools: overrides.allowed_tools.clone(),
```

（注意：`overrides` 若在此作用域不叫这个名，用该构造处实际读 `desktop_gateway`/`channel_platform` 的同一来源变量。）

(d) `manager/nomi/agent.rs`（:243 `config.tools.browser.enabled` 后）加：

```rust
        // 进程内 Spawn 门控 + per-session 工具白名单（工厂已算好；bootstrap 消费）。
        config.tools.in_process_spawn = config_extra.in_process_spawn;
        config.tools.builtin_allowlist = config_extra.allowed_tools.clone();
```

(e) **编译修复**：`cargo check -p nomifun-ai-agent 2>&1 | grep error` 找出所有 `NomiResolvedConfig { ... }` 构造点（含测试 `make_test_config`），补 `in_process_spawn: true, allowed_tools: Vec::new(),`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p nomifun-ai-agent --lib`
Expected: 全部 PASS（581+ 项）

- [ ] **Step 5: Commit**

```bash
git add crates/backend/nomifun-api-types/src/agent_build_extra.rs crates/backend/nomifun-ai-agent/src/types.rs crates/backend/nomifun-ai-agent/src/factory/nomi.rs crates/backend/nomifun-ai-agent/src/manager/nomi/agent.rs
git -c user.name=nomifun -c user.email=rika00@qq.com commit -m "feat(ai-agent): 工厂计算进程内 Spawn 门控 + per-session 工具白名单灌入引擎 config"
```

---

### Task 5: `plan_flat`（跳过 planner 的扁平落库）

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/run_service.rs`（`plan()` :262-448 拆分；新增 `plan_flat` + `spawn_plan_flat_and_start`）

**Interfaces:**
- Consumes: 既有 `PlannedTask`/`PlannedDag`（api-types）、`persist` 逻辑、`assign_task`、`planned_dag_has_cycle`。
- Produces: `pub async fn plan_flat(&self, run_id: &str, tasks: Vec<PlannedTask>) -> Result<(), AppError>`；`pub fn spawn_plan_flat_and_start(run_service: Arc<RunService>, engine: crate::engine::RunEngine, run_id: String, tasks: Vec<PlannedTask>)`。Task 7 的网关消费。

- [ ] **Step 1: 写失败测试**（找到 run_service.rs 既有 `mod tests` 里测 `plan()` 的用例——grep `async fn plan` / `fn plan_` in tests——完全沿用其 run 创建 + mock 依赖构造方式）

```rust
#[tokio::test]
async fn plan_flat_persists_tasks_assignments_and_activates() {
    // 沿用本文件 plan() 测试的完整 setup（RunService + mock repo/emitter/planner + create_adhoc 一个 supervised run）。
    // 关键差异：不 stub planner 输出 —— plan_flat 根本不会调它。
    let (svc, run_id) = setup_supervised_run().await; // ← 用实际 helper

    let tasks = vec![
        planned_task("查 A", "搜索模块 A 的用法"),          // ← 小构造函数见 Step 3
        planned_task("查 B", "搜索模块 B 的用法"),
    ];
    svc.plan_flat(&run_id, tasks).await.unwrap();

    let detail = svc.get_detail(&run_id).await.unwrap();
    assert_eq!(detail.tasks.len(), 2);
    assert!(detail.deps.is_empty(), "扁平 run 无依赖边");
    assert_eq!(detail.run.status, "running", "supervised 直接 running（autonomy 门复用）");
    for t in &detail.tasks {
        assert_eq!(t.status, "pending");
    }
    // 每个任务必须有 assignment（引擎 dispatch 需要）。
    assert_eq!(detail.assignments.len(), 2);
}

#[tokio::test]
async fn plan_flat_rejects_empty_tasks() {
    let (svc, run_id) = setup_supervised_run().await;
    let err = svc.plan_flat(&run_id, vec![]).await.unwrap_err();
    assert!(matches!(err, AppError::BadRequest(_)), "空任务列表必须拒绝，否则 run 立即 stuck");
}

#[tokio::test]
async fn plan_flat_persists_synthesis_dep_edges() {
    // 携带 depends_on 的 synthesis 任务（nomi_spawn synthesize=true 的形状）也能落边。
    let (svc, run_id) = setup_supervised_run().await;
    let mut synth = planned_task("综合", "汇总各子任务产出并标注冲突");
    synth.kind = "synthesis".to_string();
    synth.depends_on = vec![0, 1];
    synth.role = Some("reviewer".to_string());
    let tasks = vec![planned_task("A", "a"), planned_task("B", "b"), synth];
    svc.plan_flat(&run_id, tasks).await.unwrap();
    let detail = svc.get_detail(&run_id).await.unwrap();
    assert_eq!(detail.tasks.len(), 3);
    assert_eq!(detail.deps.len(), 2, "synthesis 依赖两个上游");
}
```

测试 helper（加在 tests mod 内）：

```rust
fn planned_task(title: &str, spec: &str) -> PlannedTask {
    PlannedTask {
        title: title.to_string(),
        spec: spec.to_string(),
        task_profile: None,
        depends_on: vec![],
        member_index: None,
        rationale: None,
        role: None,
        kind: "agent".to_string(),
        pattern_config: None,
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p nomifun-orchestrator --lib plan_flat`
Expected: FAIL（方法未定义）

- [ ] **Step 3: 实现拆分**——`plan()` 保持公共签名与行为**逐字节等价**：

(a) 把 `plan()` 的 :306-447（cycle guard 起，到 autonomy 门 + `emit_run_status` 止）整体搬进新的私有方法（**代码原样移动，不改一行逻辑**；`decomposing`/`planning_started` 两条 emit **留在** `plan()`；`assigning` emit 在 :390，随代码块进 helper）：

```rust
    /// plan() 的「落库半段」：cycle guard → 建任务 → 连边 → 分派 assignment →
    /// planUpdated → autonomy 门。与 planner 完全解耦，plan()（LLM 产 DAG）与
    /// plan_flat()（调用方显式任务）共用。行为与拆分前逐字节一致。
    async fn persist_dag_and_activate(
        &self,
        run_id: &str,
        run: &nomifun_db::models::OrchRunRow, // ← 用 plan() 里 run 变量的实际类型
        members: &[FleetMember],
        mut dag: PlannedDag,
    ) -> Result<(), AppError> {
        // …… :306-447 原样搬入（cycle guard 用 &run.goal；末尾 autonomy 门读 run.autonomy）……
        Ok(())
    }
```

(b) `plan()` 变为：produce 两条 emit + `self.persist_dag_and_activate(run_id, &run, &members, dag).await`。

(c) 新增：

```rust
    /// 扁平 fan-out 规划（nomi_spawn）：跳过 planner LLM，直接把调用方给的任务
    /// 列表落库并激活。任务为空 → BadRequest（否则 run 会立即被判 stuck）。
    /// depends_on（如 synthesize 汇总节点）照常落边；autonomy 门与 plan() 一致。
    pub async fn plan_flat(&self, run_id: &str, tasks: Vec<PlannedTask>) -> Result<(), AppError> {
        if tasks.is_empty() {
            return Err(AppError::BadRequest("plan_flat requires at least one task".into()));
        }
        let run = self
            .run_repo
            .get_run(run_id)
            .await
            .map_err(OrchestratorError::from)?
            .ok_or_else(|| OrchestratorError::NotFound(format!("run {run_id}")))?;
        let members: Vec<FleetMember> = decode_fleet_snapshot(run_id, &run.fleet_snapshot);
        self.persist_dag_and_activate(run_id, &run, &members, PlannedDag { tasks }).await
    }
```

(d) `spawn_plan_and_start`（:1791）旁新增：

```rust
/// nomi_spawn 的后台编排：plan_flat（无 planner）→ engine.start。扁平 run 恒为
/// 非 interactive（supervised/autonomous），故 plan_flat 成功即直接启动引擎。
/// 与 spawn_plan_and_start 同样 fail-soft：失败只 warn，run 留在 planning 可重试。
pub fn spawn_plan_flat_and_start(
    run_service: Arc<RunService>,
    engine: crate::engine::RunEngine,
    run_id: String,
    tasks: Vec<PlannedTask>,
) {
    tokio::spawn(async move {
        if let Err(err) = run_service.plan_flat(&run_id, tasks).await {
            tracing::warn!(run_id = %run_id, error = %err, "flat planning failed; run left in `planning`");
            return;
        }
        engine.start(run_id);
    });
}
```

（确认 `spawn_plan_and_start` 是否在 lib.rs re-export——是则同样 re-export `spawn_plan_flat_and_start`。）

- [ ] **Step 4: 跑测试确认通过 + plan() 无回归**

Run: `cargo test -p nomifun-orchestrator --lib`
Expected: 全部 PASS（既有 plan/adjust/engine 测试全绿）

- [ ] **Step 5: Commit**

```bash
git add crates/backend/nomifun-orchestrator/src/run_service.rs crates/backend/nomifun-orchestrator/src/lib.rs
git -c user.name=nomifun -c user.email=rika00@qq.com commit -m "feat(orchestrator): plan_flat 扁平规划（跳过 planner LLM）+ spawn_plan_flat_and_start"
```

---

### Task 6: worker 受限角色（per-node 工具白名单 + 网关收缩）

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/worker.rs`（`role_allowed_tools` + `build_worker_extra` 加 role 参数 + `run_restricted` 默认方法）
- Modify: `crates/backend/nomifun-orchestrator/src/engine.rs:1272-1283`（dispatch 调 `run_restricted` 传 `task.role`）

**Interfaces:**
- Consumes: Task 4 的 `NomiBuildExtra.allowed_tools`（extra JSON 键 `allowed_tools`）。
- Produces: `WorkerRunner::run_restricted(role, ...)` trait 默认方法（默认忽略 role 委托 `run` —— **17 处 mock 零翻修**）；`fn role_allowed_tools(role: Option<&str>) -> Option<Vec<&'static str>>`。

- [ ] **Step 1: 写失败测试**（加到 worker.rs 既有 `mod tests`，紧邻 `build_worker_extra_carries_correlation_keys_and_brief`）

```rust
#[test]
fn role_allowed_tools_maps_restricted_roles() {
    assert_eq!(role_allowed_tools(Some("searcher")), Some(vec!["Read", "Grep", "Glob"]));
    assert_eq!(role_allowed_tools(Some("Reviewer")), Some(vec!["Read", "Grep", "Glob"])); // 大小写不敏感
    assert_eq!(role_allowed_tools(Some("verifier")), Some(vec!["Read", "Grep", "Glob", "Bash"]));
    // implementer / 中文 planner 角色 / 无角色 → 不限制。
    assert_eq!(role_allowed_tools(Some("implementer")), None);
    assert_eq!(role_allowed_tools(Some("前端")), None);
    assert_eq!(role_allowed_tools(None), None);
}

#[test]
fn build_worker_extra_restricted_role_shrinks_tools_and_gateway() {
    // 受限角色：带 allowed_tools 白名单，且不授桌面网关（只读 worker 不该有全桌面控制）。
    let extra = build_worker_extra("run_abc", "task_xyz", "brief", None, None, &[], &[], Some("searcher"));
    assert_eq!(extra["desktopGateway"], false, "受限角色不得静默升权到全量网关");
    assert_eq!(extra["allowed_tools"], serde_json::json!(["Read", "Grep", "Glob"]));
    // 无角色/implementer：现状不变（网关 + 无白名单）。
    let full = build_worker_extra("run_abc", "task_xyz", "brief", None, None, &[], &[], None);
    assert_eq!(full["desktopGateway"], true);
    assert!(full.get("allowed_tools").is_none());
}
```

（既有的 `build_worker_extra_carries_correlation_keys_and_brief` 调用点补第 8 个参数 `None`。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p nomifun-orchestrator --lib role_allowed_tools`
Expected: FAIL

- [ ] **Step 3: 实现**

(a) worker.rs 加映射（语义与进程内 Spawn 的 `role_tools` 一致）：

```rust
/// 受限角色 → 工具白名单（与进程内 Spawn 的 role_tools 语义一致）。
/// None = 不限制（implementer / planner 的中文角色标签 / 无角色）。
fn role_allowed_tools(role: Option<&str>) -> Option<Vec<&'static str>> {
    match role.map(|r| r.to_ascii_lowercase()).as_deref() {
        Some("searcher" | "scout" | "reviewer") => Some(vec!["Read", "Grep", "Glob"]),
        Some("verifier" | "tester") => Some(vec!["Read", "Grep", "Glob", "Bash"]),
        _ => None,
    }
}
```

(b) `build_worker_extra` 加尾参 `role: Option<&str>`，函数体改：

```rust
    let restricted = role_allowed_tools(role);
    let mut extra = json!({
        "session_mode": "yolo",
        // 受限角色不授桌面网关（只读 worker 拿全桌面控制 = 静默升权）；
        // 其余 worker 保持现状全量网关。
        "desktopGateway": restricted.is_none(),
        "orchestrator_run_id": run_id,
        "orchestrator_task_id": task_id,
        "system_prompt": brief,
        "preset_enabled_skills": enabled_skills,
        "exclude_auto_inject_skills": disabled_builtin_skills,
    });
    if let Some(tools) = restricted {
        extra["allowed_tools"] = json!(tools);
    }
```

（其余 persona/workspace 分支不动；`ConversationWorkerRunner::run` 内的调用点补 `None`——见 (c)。）

(c) trait 加默认方法（`WorkerRunner` 内，`run` 之后）：

```rust
    /// 带角色的执行入口：默认忽略 role 委托给 [`Self::run`]（mock/测试 runner
    /// 零改动）。生产 [`ConversationWorkerRunner`] 覆写它把 role 映射为
    /// per-node 工具白名单 + 网关收缩。引擎 dispatch 统一走本方法。
    #[allow(clippy::too_many_arguments)]
    async fn run_restricted(
        &self,
        role: Option<&str>,
        member: &FleetMember,
        workspace_dir: Option<&str>,
        run_id: &str,
        task_id: &str,
        brief: &str,
        task_spec: &str,
        timeout: Duration,
        on_started: Box<dyn FnOnce(i64) + Send>,
    ) -> Result<WorkerOutcome, AppError> {
        let _ = role;
        self.run(member, workspace_dir, run_id, task_id, brief, task_spec, timeout, on_started)
            .await
    }
```

(d) `ConversationWorkerRunner`：把现 `run` 的函数体整体改造成 `run_restricted` 的覆写（`build_worker_extra(..., role)` 传真 role），`run` 本体变一行委托：

```rust
    async fn run(&self, member: &FleetMember, workspace_dir: Option<&str>, run_id: &str, task_id: &str, brief: &str, task_spec: &str, timeout: Duration, on_started: Box<dyn FnOnce(i64) + Send>) -> Result<WorkerOutcome, AppError> {
        self.run_restricted(None, member, workspace_dir, run_id, task_id, brief, task_spec, timeout, on_started).await
    }
```

(e) engine.rs :1272 `worker.run(` 改 `worker.run_restricted(task_role.as_deref(),`——`task_role` 在 future 构造前克隆：`let task_role = task.role.clone();`（:1235 一排克隆处）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p nomifun-orchestrator --lib`
Expected: 全部 PASS（mock runner 走默认方法，零翻修）

- [ ] **Step 5: Commit**

```bash
git add crates/backend/nomifun-orchestrator/src/worker.rs crates/backend/nomifun-orchestrator/src/engine.rs
git -c user.name=nomifun -c user.email=rika00@qq.com commit -m "feat(orchestrator): 受限角色 worker（per-node 工具白名单 + 网关收缩），run_restricted 默认方法零 mock 翻修"
```

---

### Task 7: `nomi_spawn` 网关能力

**Files:**
- Modify: `crates/backend/nomifun-gateway/src/caps_orchestrator.rs`（新 params/handler/注册）

**Interfaces:**
- Consumes: Task 5 `plan_flat`/`spawn_plan_flat_and_start`；既有 `resolve_model_range`/`read_conversation_model_range`/`expand_auto_range`/`build_adhoc_request`/`parse_lead_conv_id`/`require_user`/`ok`。
- Produces: MCP 工具 `nomi_spawn`（tasks[{name,prompt,role?}], synthesize?）。

- [ ] **Step 1: 写注册测试**（加到 caps_orchestrator.rs 既有 `mod tests`；先 grep 该 mod 现有的注册断言写法——如按名字找 Capability 并断言 deny surface——完全沿用）

```rust
#[test]
fn nomi_spawn_registered_and_denied_on_remote() {
    let mut caps = Vec::new();
    register(&mut caps);
    let spawn = caps.iter().find(|c| c.meta().name == "nomi_spawn").expect("nomi_spawn registered");
    // 与 nomi_run_create 一致：Remote 面必须拒绝。（断言方式沿用本 mod 既有 deny 测试。）
    assert!(spawn.meta().denied_on(Surface::Remote));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p nomifun-gateway --lib nomi_spawn`
Expected: FAIL

- [ ] **Step 3: 实现**——params（`RunResultParams` 之后）：

```rust
/// One task in a `nomi_spawn` flat fan-out.
#[derive(Deserialize, JsonSchema)]
struct SpawnTaskParam {
    /// Short descriptive name (becomes the task/node title on the canvas).
    name: String,
    /// The instruction for this sub-agent.
    prompt: String,
    /// Optional restricted role: searcher/reviewer (read-only) or verifier
    /// (read-only + Bash). Omit for full tools.
    #[serde(default)]
    role: Option<String>,
}

/// Fan several independent sub-agent tasks out in parallel — no planner, no
/// approval gate; each task runs as a visible worker on the orchestration canvas.
#[derive(Deserialize, JsonSchema)]
struct SpawnParams {
    /// 1-8 independent tasks to run in parallel.
    tasks: Vec<SpawnTaskParam>,
    /// When true (and ≥2 tasks), append a read-only synthesis task that
    /// consolidates all outputs and flags conflicts.
    #[serde(default)]
    synthesize: Option<bool>,
}
```

handler（`result` 之后；模型解析段直接照抄 `create` 的 1/1b/2 三步——不含 role_members/assistants）：

```rust
const MAX_SPAWN_TASKS: usize = 8;

async fn spawn(deps: Arc<GatewayDeps>, ctx: crate::deps::CallerCtx, p: SpawnParams) -> Value {
    let user = match require_user(&ctx) {
        Ok(u) => u.to_owned(),
        Err(e) => return e,
    };
    if p.tasks.is_empty() {
        return json!({ "error": "nomi_spawn requires at least one task" });
    }
    if p.tasks.len() > MAX_SPAWN_TASKS {
        return json!({ "error": format!("too many tasks: {} (max {MAX_SPAWN_TASKS})", p.tasks.len()) });
    }

    // 模型解析：与 create 相同的「显式 > 会话策展 > Auto 全量展开」链。
    let model_range = match read_conversation_model_range(&deps, &user, &ctx.conversation_id).await {
        Some(range) => range,
        None => ModelRange::Auto,
    };
    let lead_model: Option<ModelRef> = match &model_range {
        ModelRange::Single { model } => Some(model.clone()),
        ModelRange::Range { models } => models.first().cloned(),
        ModelRange::Auto => None,
    };
    let summaries = match load_provider_summaries(&deps).await {
        Ok(s) => s,
        Err(e) => return e,
    };
    let model_range = if matches!(model_range, ModelRange::Auto) {
        match expand_auto_range(&summaries) {
            Ok(r) => r,
            Err(e) => return e,
        }
    } else {
        model_range
    };

    let lead_conv_id = parse_lead_conv_id(&ctx.conversation_id);
    let n = p.tasks.len();
    let goal = format!("并行执行 {n} 个子任务：{}", p.tasks.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join("、"));
    // 扁平扇出恒 supervised：即时并行、无审批门（不同于 create 的 interactive 默认）。
    let req = build_adhoc_request(goal, None, model_range, "supervised".to_string(), Vec::new(), lead_conv_id, lead_model);

    let run = match deps.orchestrator_run_service.create_adhoc(&user, req).await {
        Ok(run) => run,
        Err(e) => return json!({ "error": e.to_string() }),
    };

    if !ctx.conversation_id.is_empty() {
        if let Err(e) = deps.conversation_service.link_orchestrator_run(&ctx.conversation_id, &run.id).await {
            tracing::warn!(error = %e, run_id = %run.id, "failed to link flat run to calling conversation");
        }
    }

    // 组装扁平任务（+ 可选只读综合节点）。
    let mut tasks: Vec<PlannedTask> = p
        .tasks
        .into_iter()
        .map(|t| PlannedTask {
            title: t.name,
            spec: t.prompt,
            task_profile: None,
            depends_on: vec![],
            member_index: None,
            rationale: None,
            role: t.role,
            kind: "agent".to_string(),
            pattern_config: None,
        })
        .collect();
    if p.synthesize.unwrap_or(false) && n >= 2 {
        tasks.push(PlannedTask {
            title: "综合汇总".to_string(),
            spec: "综合上游各子任务的产出为一份结论，显式标注子任务之间的冲突或分歧（无则写「无」）。".to_string(),
            task_profile: None,
            depends_on: (0..n).collect(),
            member_index: None,
            rationale: None,
            role: Some("reviewer".to_string()),
            kind: "synthesis".to_string(),
            pattern_config: None,
        });
    }

    nomifun_orchestrator::spawn_plan_flat_and_start(
        deps.orchestrator_run_service.clone(),
        deps.orchestrator_run_engine.as_ref().clone(),
        run.id.clone(),
        tasks,
    );
    ok(json!({
        "run_id": run.id,
        "status": "running",
        "task_count": n,
        "message": "子任务已在编排画布并行执行（用户可实时看到每个子 agent 的状态与产出）。用 nomi_run_status 跟进、nomi_run_result 取汇总，然后向用户总结。",
    }))
}
```

（`read_conversation_model_range` 的实际返回类型以文件为准——若是 `Option<ModelRange>` 按上；若模式不同，照 `create` :140-146 的用法适配。`ModelRef`/`PlannedTask` 的 import 按需补。）

注册（`register()` 里 `nomi_run_create` 之后）：

```rust
    // 1b. Flat fan-out（write）。与 create 同域同 deny：Desktop-only。
    out.push(Capability::new::<SpawnParams, _, _>(
        CapabilityMeta::new(
            "nomi_spawn",
            "orchestrator",
            "Run several INDEPENDENT sub-agent tasks in parallel with live visualization (each task = a visible worker on the orchestration canvas; the user sees status and output per agent). No planner, no approval gate — starts immediately. Params: tasks (1-8 of {name, prompt, role?}; role searcher/reviewer = read-only, verifier = read-only+Bash, omit = full tools), synthesize (optional; true appends a read-only consolidation task). Use nomi_run_create instead for complex goals needing decomposition/dependencies. Returns run_id; follow up with nomi_run_status / nomi_run_result.",
            DangerTier::Write,
        )
        .deny_on(ORCHESTRATOR_DENY_SURFACES),
        |deps, ctx, p| spawn(deps, ctx, p),
    ));
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p nomifun-gateway --lib`
Expected: 全部 PASS

- [ ] **Step 5: Commit**

```bash
git add crates/backend/nomifun-gateway/src/caps_orchestrator.rs
git -c user.name=nomifun -c user.email=rika00@qq.com commit -m "feat(gateway): nomi_spawn 扁平并行扇出能力（画布可视化、supervised 即跑、deny Remote）"
```

---

### Task 8: Prompt 引导（lead + 伙伴）

**Files:**
- Modify: `crates/backend/nomifun-ai-agent/src/factory/nomi.rs`（`LEAD_ORCHESTRATOR_PROMPT` ~:664）
- Modify: `crates/backend/nomifun-companion/src/companion.rs`（智能编排 nudge，grep `调度子 agent`）

**Interfaces:** 纯文案；既有测试断言 `nomi_run_create` 在 prompt 里，新增断言 `nomi_spawn`。

- [ ] **Step 1: 更新既有 prompt 测试**——factory 测试断言 lead prompt 含 `nomi_spawn`；companion 测试 `companion_system_prompt_smart_orchestration_nudge_local_only` 加断言 `on.contains("nomi_spawn")`。跑确认 FAIL。

- [ ] **Step 2: 改文案**

`LEAD_ORCHESTRATOR_PROMPT` 在「对复杂、可拆分…」句后补一句：

```
对多个相互独立、无需拆解的并行小任务：改用 `nomi_spawn(tasks)` 直接并行扇出（无需规划、立即执行、每个子任务在画布上可见）。
```

Companion nudge（`调度子 agent（智能编排）` 段）在 `nomi_run_create` 句后补：

```
如果只是几个相互独立的小任务要并行跑，用 nomi_spawn(tasks) 更快：不经规划直接开工，主人能在画布上看到每个子任务的进展。
```

- [ ] **Step 3: 跑测试确认通过**

Run: `cargo test -p nomifun-ai-agent --lib lead && cargo test -p nomifun-companion --lib companion::`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/backend/nomifun-ai-agent/src/factory/nomi.rs crates/backend/nomifun-companion/src/companion.rs
git -c user.name=nomifun -c user.email=rika00@qq.com commit -m "feat(prompt): lead/伙伴提示引导独立并行小任务用 nomi_spawn"
```

---

### Task 9: 全量回归 + 设计文档状态更新

**Files:**
- Modify: `docs/superpowers/specs/2026-07-02-subagent-visualization-unification-design.md`（追加实施状态节）

- [ ] **Step 1: 全量回归**

```bash
cargo test -p nomi-tools -p nomi-config -p nomi-agent --lib
cargo test -p nomifun-orchestrator -p nomifun-gateway -p nomifun-ai-agent -p nomifun-companion -p nomifun-conversation --lib
cargo check --workspace --message-format=short
bun run typecheck && bun run check:i18n
```

Expected: 全部 exit 0。前端无改动，typecheck/i18n 应原样通过。

- [ ] **Step 2: 设计文档追加「实施状态」**（列已交付各 Task、测试数、明确「运行时画布点亮」需真机验证——冒烟步骤：桌面会话让模型调 `nomi_spawn` 两个 hello-world 任务 → 右栏编排 tab 出现 2 节点 → 点节点看 worker 转录 → `nomi_run_result` 有汇总）。

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-07-02-subagent-visualization-unification-design.md
git -c user.name=nomifun -c user.email=rika00@qq.com commit -m "docs: 子 agent 可视化统一 v1 实施状态与真机冒烟清单"
```

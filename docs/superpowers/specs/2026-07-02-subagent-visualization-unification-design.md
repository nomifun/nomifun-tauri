# 子 Agent 可视化底座统一：把「智能编排」作为所有 subagent 的执行 + 可视化底层

日期：2026-07-02
状态：草案（recon 已验证可行性，方案 A；自主实施 scoped v1）
分支：`feature/partner-memory-and-orchestration`

> 用户诉求（原话要点）：启用 `Spawn` 后子 agent「完全堵住、没有任何输出」，没有可视化预览，体验追不上商业产品。而「智能编排」迭代时其实已经把 worker 会话化 + DAG 画布 + 决策流 + worker 转录投射这套可视化做出来了。**希望把智能编排的能力底层化，作为以后所有子 agent 的可视化交互底层**，让用户清晰看到每个 agent 的状态、在做什么、产出了什么。

---

## 0. 根因与结论

**为什么 Spawn「堵住、没输出」**：引擎内 `AgentSpawner`（`crates/agent/nomi-agent/src/spawner.rs`）给每个子 agent 用 `NullSink`——不落库、不流式、无 `conversation_id`、不发任何事件；`spawn_parallel` **同步**等所有子 agent 跑完（每个上限 300s）才把结果作为**一条** `tool_result` 返回。所以主会话全程静默、像卡死。前端**根本没有可订阅的东西**。

**为什么编排器天然有可视化**：`nomifun-orchestrator` 的每个任务节点 = 一条**真实 nomi worker 会话**（`ConversationWorkerRunner`，落库 + 流式 + `conversation_id` + WS 事件 + `orchestrator_task_id` 关联），前端 `DagCanvas` / `RunDecisionFeed` / `ProjectedWorkerView` 整套可视化已就绪，且**纯数据驱动、与 planner 无关**。

**结论**：把编排器作为子 agent 的统一执行 + 可视化底座（方案 A）。桌面会话的「快速并行扇出」不再走静默的进程内 Spawn，而是走一个**扁平 fan-out run**（跳过 planner LLM 的 N 个独立 worker 任务），link 回当前会话 → 复用整套编排可视化。进程内 Spawn 仅保留给 CLI / 独立模式。

recon（6 路并行 + 严格评审，实测核对关键行）裁定：**needs-work，无阻塞项**——5 条集成缝 4 条机械上 clean、引擎零改动即可执行无 planner 扁平 run、前端可原样点亮；被两件**设计决策**（非机械缝）拉到 needs-work，v1 已针对性处理（见 §4）。

---

## 1. 术语

- **进程内 Spawn**：引擎 `SpawnTool` + `AgentSpawner`，同进程子 `AgentEngine`、`NullSink`、同 cwd。轻、无持久化、无可视化。
- **扁平 fan-out run（新）**：一个编排 run，N 个**无依赖**的根任务，**跳过 planner LLM**，每个任务 = 一条 worker 会话。复用编排可视化。
- **编排 run（现有）**：`nomi_run_create(goal)` → planner LLM 拆 DAG → 有依赖/聚合节点。

三者共用同一 `RunEngine` 执行 + 同一前端可视化。

---

## 2. 架构：三层，统一在「run + worker 会话」

```
调用方（伙伴会话 / 普通桌面会话 / 未来任何 agent）
   │  MCP 网关工具（桌面授予，deny Remote）
   ├── nomi_run_create(goal)   → planner LLM 拆 DAG（复杂任务，已存在）
   └── nomi_spawn(tasks[])     → 扁平 fan-out，跳过 planner（简单并行，新增）★
        │
        ▼
   RunService.plan_flat(run_id, tasks)  ← 新增：复用 plan() 的落库半段，跳过 produce()
        │  link_orchestrator_run(conversation_id, run_id)  ← 复用既有写点（只 merge extra + 广播）
        ▼
   RunEngine.run_loop  ← 零改动：N 个无依赖任务被 list_ready_tasks 立即判就绪、按 max_parallel 扇出
        │  每任务 = ConversationWorkerRunner → 一条真实 nomi worker 会话（落库/流式/conversation_id/WS）
        ▼
   前端（零改动或轻打磨）：extra.orchestrator_run_id → DagCanvas + worker 转录投射 + 决策流
```

**关键不变量（recon 实测保持，绝不破坏）**：
- per-run 锁不跨 LLM await（`dispatch_task` 在 fill 段、不在终态检查锁内）；
- `link_orchestrator_run` 只 `update_extra(orchestrator_run_id)` + `broadcast_list_changed`；
- 侧栏过滤键 `orchestrator_task_id`（worker 隐藏、lead 可见）；
- Remote deny（新网关能力必须 `.deny_on(ORCHESTRATOR_DENY_SURFACES)`）；
- 无 IR / 节点图复活。

---

## 3. 后端改动

### 3.1 `plan_flat`（跳过 planner 的扁平落库）— `nomifun-orchestrator/src/run_service.rs`

`plan()`（:262-448）唯一的 LLM 耦合是 `:288 self.planner.produce(...)`；其后 :294-447（cycle guard → 建任务 → 连边 → assignment → planUpdated → autonomy 门）只对一个 `PlannedDag` 操作、与 planner 无关。

- 抽出私有 helper `persist_dag_and_activate(run_id, &run, &members, dag: PlannedDag)` = plan() 的落库半段。
- `plan()` = `produce()` + helper（行为不变）。
- 新增 `pub async fn plan_flat(run_id, tasks: Vec<PlannedTask>)`：构造 `depends_on` 全空的 `PlannedDag` → 调 helper，**完全跳过 produce()**。空 `tasks` → `BadRequest`（类比 create_adhoc 拒空 range）。
- 新增 `spawn_plan_flat_and_start`（`plan_flat` → `engine.start`，沿用 fail-soft 不 panic）。
- **lead-thinking emit 归位**：`planning_started/decomposing`（给「有 LLM 规划」的画布叙事用）留在 produce 半段；`assigning`/`emit_run_plan_updated`/autonomy 门/`emit_run_status` 进 helper（否则扁平 run 画布不刷新或误播「规划中」）。

### 3.2 `nomi_spawn` 网关能力 — `nomifun-gateway/src/caps_orchestrator.rs`

以 `nomi_run_create` 为模板，新增第 4 个能力：
- 输入 schema：`tasks: [{name, prompt, role?}]`（对齐用户熟悉的 Spawn 形状），可选 `synthesize`。
- 读 `ctx.conversation_id`（空串守卫）；`create_adhoc` 用会话的模型策展成员（lead_model + 协作模型 model_range）；随后 `plan_flat` 落任务、`link_orchestrator_run`、`spawn_plan_flat_and_start`。
- **autonomy 显式 `supervised`/`autonomous`**（不用桌面前门默认的 `interactive`）——否则会 park 到 `awaiting_plan_approval`，破坏 Spawn「即时并行、无审批」语义。这是 recon 点名的关键决策。
- `.deny_on(ORCHESTRATOR_DENY_SURFACES)`（Remote 拒绝，与现有三能力一致）。

### 3.3 引擎内 Spawn 门控 — `nomi-agent/src/bootstrap.rs` + config + factory

- `nomi-config/src/config.rs` `ToolsConfig` 加 `#[serde(default = default_true)] in_process_spawn: bool`（Default = true，保 CLI）。
- `bootstrap.rs:580-593`：`if self.config.tools.in_process_spawn { register(SpawnTool) }`（`ToolRegistry` 无 unregister，必须注册前拦截）。
- `NomiResolvedConfig`（types.rs）+ `factory/nomi.rs`：桌面**网关**会话（`desktop_gateway` 且非 companion/remote）置 `false` → 改走 `nomi_spawn`；`manager/nomi/agent.rs` 紧邻 browser/computer 开关灌入。**须同步补齐所有 `make_test_config`/`NomiAgentManager::new` 调用点**（漏改编译失败）。
- CLI / 独立模式 / 子 agent 独立 registry 不受影响（子 agent registry 本就不含 Spawn）。
- `LEAD_ORCHESTRATOR_PROMPT` 及伙伴智能编排提示：引导「复杂/多步用 `nomi_run_create`，多个独立小任务并行用 `nomi_spawn`」，避免模型退回 Bash 手搓并行。

### 3.4 能力对等：per-node 工具白名单（安全必须，非可选）

recon 指出：编排 worker 恒 `desktopGateway:true` 全量工具。若把 Spawn 的 `role`（searcher/reviewer 只读、verifier 只读+Bash）直接映射成节点却不收缩工具，等于**把只读角色静默升权成可写可执行**——这是安全退化，v1 必须堵。

- `PlannedTask` / `RunTask` 增 per-node 工具白名单（由 `role` 推导）；`worker.rs build_worker_extra` 落成工具收缩（沿用 Spawn `role_tools` 的映射：searcher/reviewer=Read/Grep/Glob、verifier=+Bash、implementer=全量）。
- 落点：worker `extra` 传一个 `allowed_tools`，工厂据此过滤引擎工具（引擎已有 plan-mode 的工具过滤机制可参照）。

---

## 4. 范围（v1）与明确取舍

**v1 纳入**：
- `plan_flat` + `nomi_spawn` 网关能力（supervised 自主度、deny Remote）；
- 引擎 Spawn 门控（桌面网关会话改走 `nomi_spawn`，CLI 保留）；
- per-node 工具白名单（堵住只读角色静默升权——安全必须）；
- prompt 引导；
- 前端：验证可视化原样点亮（预计零改动）；可选打磨扁平 run 空态文案。

**v1 暂不做（记录，非阻塞）**：
- **worktree 隔离**（Spawn 的 `isolate`：每子 agent 独立 git worktree + diff 回收）。编排 worker 共享单一 workspace，多个并行 implementer 改同文件会互相覆盖。v1 **显式约束**：`nomi_spawn` 用于研究/分析/只读扇出，或写入**互不重叠**文件的独立任务；并行改同一文件的隔离场景暂留进程内 Spawn（CLI）或后续 v2 给编排补 per-node worktree。UI/prompt 明示此约束。
- **coordinate/TaskBoard 的运行时动态领取**（活体兄弟 work-steal）。编排用 DAG 静态切分替代，v1 标暂不支持。
- 大 N 扁平 run 的画布换行/网格布局（当前单行铺开，功能正常观感差）——可选打磨。

**取舍理由**：v1 覆盖 Spawn 最常见用途（并行研究/分析/独立产出）并**从根上解决「静默卡死」**，同时避开两个会引入数据损坏/静默升权的风险项，把它们收窄到明确边界或 v2。

---

## 5. 前端

recon 实测：可视化链路纯数据驱动、plan 无关，扁平 run 可原样点亮。**预计零改动**，仅验证 + 可选打磨：
- 硬前提：被链接会话 `type==='nomi'`（`useConversationRun.ts:46`）——桌面会话满足。
- worker 隐藏过滤（`orchestrator_task_id`）已就绪，扁平 run worker 天然带键。
- 可选打磨（非阻塞）：`DagCanvas` 空态「规划中…」文案对无 planner 扁平 run 不贴切；`layoutDag` 大 N 单行铺开。

---

## 6. 回归面

- 编排不变式（§2）全部保持——recon 已实测核对。
- 跨 crate 新字段（`in_process_spawn`、per-node `allowed_tools`）漏改测试构造 → 编译失败（低风险但要全覆盖）。
- `plan()` 抽 helper 时 lead-thinking emit 分错半段 → 画布叙事异常（§3.1 已标注）。
- 门控条件配错（一刀切禁所有桌面 Spawn + 替代前门默认 interactive）→「删并行能力 + 加批准门」双重回归（§3.2/3.3 已规避）。
- 上一提交的 `coerce_input_to_schema`（Spawn 入参字符串化容错）对 `nomi_spawn` 同样生效。
- 回归验证：`nomi-agent` / `nomi-tools` / `nomifun-orchestrator` / `nomifun-gateway` / `nomifun-ai-agent` cargo test；`cargo check --workspace`；前端 typecheck/i18n/build。

---

## 7. 验收标准

1. 桌面会话（伙伴 + 普通）里模型发起并行子任务时，**不再静默卡死**：立刻出现可视化（DAG 画布 / 节点状态 / 可点击 worker 转录），能看到每个 agent 的状态、在做什么、产出了什么。
2. `nomi_spawn` 走 supervised 自主度即时执行、不停在批准门。
3. 只读角色（searcher/reviewer）的 worker 确实只有只读工具（无静默升权）。
4. CLI / 独立模式的进程内 Spawn 不受影响。
5. Remote 会话拒绝 `nomi_spawn`。
6. 既有 `nomi_run_create` 编排、AutoWork、idmm、知识库全部照常。
7. 全部回归命令绿。

## 8. 实施状态（2026-07-02，scoped v1 已交付）

按 `docs/superpowers/plans/2026-07-02-subagent-visualization-unification.md` 全部 9 任务完成，每任务独立提交：

- **Task 1** `nomi-tools`：`ToolRegistry::retain_named`（空=不限制）。
- **Task 2** `nomi-config`：`ToolsConfig.in_process_spawn`（默认 true）+ `builtin_allowlist`（默认空）；global/project 两条合并分支同步。
- **Task 3** `nomi-agent` bootstrap：SpawnTool 注册门控 + 全部注册后 `retain_named` 收口（含 MCP 代理、ToolSearch 快照之前）。
- **Task 4** `nomifun-ai-agent`：`NomiBuildExtra.allowed_tools` → `NomiResolvedConfig{in_process_spawn, allowed_tools}` → manager 灌 config；纯函数 `engine_spawn_enabled(desktop_gateway, channel_platform)`（本地桌面网关会话禁进程内 Spawn，IM 渠道保留）。
- **Task 5** `nomifun-orchestrator`：`plan()` 拆出 `persist_dag_and_activate`（行为逐字节等价）；`plan_flat`（空拒绝、synthesis 依赖边/role 持久化）；`spawn_plan_flat_and_start`。
- **Task 6** `nomifun-orchestrator` worker：`role_allowed_tools`（searcher/reviewer=只读、verifier=+Bash、中文角色/无角色=不限制）；受限角色 `desktopGateway:false` + `allowed_tools`；trait 默认方法 `run_restricted`（17 处 mock 零翻修）；引擎 dispatch 传 `task.role`。
- **Task 7** `nomifun-gateway`：`nomi_spawn` 能力（tasks 1-8、role、synthesize→只读 synthesis 节点、**显式 supervised**、link 回调用会话、后台 plan_flat→start、`.deny_on(Remote)`）。
- **Task 8** prompts：`LEAD_ORCHESTRATOR_PROMPT` + 伙伴智能编排 nudge 增 `nomi_spawn` 引导。

**回归（全绿）**：nomi-tools 168 / nomi-config 135 / nomi-agent 444+bootstrap 11 / nomifun-ai-agent 582 / companion 207 / conversation 254 / gateway 78 / orchestrator 281（另有 1 项 `cap_2_total_elapsed_reflects_overlap_not_serial_sum` 为**既有**时间敏感 flaky——已用 git stash 在未改动基线上复现同样失败，与本次无关）；`cargo check --workspace`、前端 `typecheck`/`check:i18n` 通过（前端零改动）。

**真机冒烟清单（待用户回来验证运行时画布）**：
1. 桌面普通会话让模型调 `nomi_spawn`（如「用 nomi_spawn 并行跑两个子任务：各自输出 hello world」）→ 工具立即返回 run_id，**主会话不卡**。
2. 右栏「编排」tab / 会话头 pill 点亮 → DAG 画布出现 2 个节点，状态从 pending→running→done 实时变化。
3. 点节点 → 内容区投射该 worker 的实时转录；点 main 返回主会话。
4. `nomi_run_result` 有各任务产出；带 `synthesize:true` 时多一个「综合汇总」节点。
5. 伙伴会话开启智能编排后同样可用；受限角色任务（role=searcher）的 worker 会话无 Write/Bash 工具。
6. CLI（nomi-cli）里 `Spawn` 工具照常注册可用。

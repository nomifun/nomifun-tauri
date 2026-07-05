# Phase 3 设计草案 — 动态主 agent + 自主长循环 + 受控嵌套

> 日期：2026-07-04 ｜ 状态：**设计草案，待用户审阅（尤其 §2 的五个分叉）** ｜ 未实现
> 这是统一编排 spec（`2026-07-04-subagent-standardization-and-unified-orchestration-design.md`）§7 的实现级细化。Phase 0/1a/1b/2 已实现完成（stacked 分支未合）。Phase 3 是最大最核心一相，动引擎核心 dispatch 循环，故先出设计 + 定分叉再实现。

## 0. 目标（回顾）

把「lead 会话 = 主 agent」做实，覆盖 Claude Code 式交互：主控随时派发 subagent、拿回结果继续推理、按需再派发；并支持**自主长循环**（带目标+退出条件自跑、无需用户逐轮）与**受控嵌套**（subagent 再委派）。骨架式（Phase 0/1 已有的 `nomi_run_create` 静态 DAG + 画布）与动态式共用一套引擎——区别只是「节点是提前铺满还是增量涌现」。

## 1. 现状与可复用的缝（勘察已坐实）

- **运行时长 DAG 结构上已可工作**：`run_loop` 每轮重查 `list_ready_tasks`，`RunLocks` 保证「追加节点」与「终态判定」原子互斥、loop 在锁内注销 handle。唯一阻塞 = `adjust`/append 现要求「无节点在跑」（`compute_/apply_adjusted_plan` 的 `no task running` 拒绝）。
- **中途观测已开头**：Phase 1a 已加 `RunOutcome::NodeFailed` 中途回执（节点永久失败时唤起 lead）。成功侧的中途观测尚无。
- **`autonomous` 自主模式是空槽**：现仅为 `supervised` 同义词（无分支区分），正好可赋真语义。
- **AutoWork 持久循环是现成外壳**：`nomifun-requirement/src/orchestrator.rs` 的 `run_loop`（`claim→inject→wait→finalize→idle-on-wake→repeat` + boot-resume + sweeper + 退避），引擎注释自承「RunEngine 是 AutoWork 的忠实缩减版」。
- **子委派现不安全**：worker `extra.orchestrator_run_id`/`_task_id` 仅关联标记无人消费；worker 调 `nomi_run_create` 建**孤儿子 run**（挂一次性 worker 会话，终态回执落已结束会话）；**无深度守卫**；只读受限角色 `desktopGateway=false` 本就无网关工具。
- **每会话可能多 run**：现 `nomi_spawn`/`nomi_run_create` 每次调用 `create_adhoc + link`，一个会话多次派发会建多个 run 并重写 `extra.orchestrator_run_id`。

## 2. 五个设计分叉（★需用户确认；下列为推荐默认）

### F1 — 自主循环的退出条件与「目标达成」判定 ★
**问题**：自主 master 循环什么时候停？谁判定目标已达成？
- **推荐默认**：多重退出条件（任一触发即停）：① lead LLM 自评「目标已达成」（每轮观测后由 master 自己判断并显式声明完成）；② token/轮次预算耗尽；③ 连续 N 轮（默认 3）无实质进展（无新节点、无新产出）；④ 用户中止。**目标达成判定 = master 自评为主 + 可选插一个 verify/acceptance 节点（复用 Phase 1a 的验收门）做客观校验**。
- 备选：纯预算驱动（简单但可能过早/过晚停）；纯 verify 节点驱动（客观但重）。

### F2 — 嵌套策略（深度 / 是否嫁接父图 / 谁可委派）★
**问题**：subagent 能否再派 subagent？多深？子 run 独立还是并入父图？
- **推荐默认**：① **深度上限默认 2**（master→sub→subsub 即止），worker `extra` 传递并消费 `delegation_depth`，超限拒绝再派发并如实告知；② **子委派并入父 run 的图**（受控 append，父 run 单一画布可视化，避免孤儿），而非建独立子 run；③ 只 **full-role**（非只读受限角色）且深度未超限可再委派；只读角色（searcher/reviewer/verifier）继续禁派发。
- 备选：深度 1（不允许嵌套，最安全但弱）；受管独立子 run（需给子 run 终态活跃重唤起路径，更复杂）。

### F3 — 单会话单持久 run 收敛 ★
**问题**：现每次派发建新 run；动态 master 要「一个会话一个持续生长的 run」。如何收敛 + 兼容既有多 run 会话？
- **推荐默认**：会话已有 `orchestrator_run_id` 时，后续派发**追加到现有 run**（经新 `add_tasks` 原语），而非新建；画布始终展示这一个持续生长的 run。既有多 run 历史会话只读兼容（不迁移，新会话走单 run）。
- 备选：保持多 run + 画布聚合展示多 run（改动小但画布语义碎、跨 run 依赖难表达）。

### F4 — 中途观测（成功侧）的节奏 ★
**问题**：master 多久被再唤起一次观测？（成本 vs 及时性）
- **推荐默认**：**按批次**——一轮 fill 的在飞节点全部 settle（或达到某数量）后回执一次给 lead（而非每节点一次），附本批产出摘要；失败仍即时（Phase 1a 已有 NodeFailed）。节流避免每节点唤起 lead 的 token 爆炸。
- 备选：每关键节点即时（更及时更贵）；仅终态（=退回非动态，不采用）。

### F5 — 动态 master 是否默认标配 ★
**问题**：Phase 0 已把 subagent 能力标配化。动态 master 行为（可随时派发/追加/自主循环）是默认开，还是显式启用？
- **推荐默认**：**随时派发/追加 = 默认标配**（承接 Phase 0，基础提示词已引导）；**自主长循环 = 显式启用**（用户明确要「自跑到目标」时开，因它长时间无人值守、耗预算，需知情同意）；嵌套子委派默认开但受 F2 深度约束。
- 备选：动态 master 也需显式开（更保守但削弱「标配」价值）。

## 3. 架构与实现骨架（分叉确认后细化为 plan）

### W7a — 单会话单持久 run + 运行时追加（F3 + 运行时 DAG 增长）
- 新增控制面原语 `add_tasks(run_id, tasks)`：复用 `plan_flat`/`reconcile_run_plan` 的插入逻辑把 caller 指定节点追加到**现有 run**，走同一 `assign_task` 路由；存活 loop 下轮 `list_ready_tasks` 自取，或 re-activate 终态 run。
- **放宽 `adjust`/append 的「无节点在跑」限制**（`run_service.rs` `compute_/apply_adjusted_plan`）：`RunLocks` 已保证追加与终态判定原子，故允许边跑边追加。这是「边跑边派发」唯一阻塞点。
- 网关给 lead 暴露「向本 run 追加子任务」工具（区别于新建 run 的 `nomi_spawn`）；会话已有 run 时 `nomi_spawn`/`nomi_run_create` 改为追加语义。

### W7b — 动态 master 中途观测（F4）
- 在 `settle_task_outcome` 后 / 每 fill 批次末，节流地经 `deps.lead_reporter` 发**批次完成观测回执**（复用 Phase 1a 的 `LeadReporter` 缝，新增 `RunOutcome::BatchProgress` 或复用带摘要的通道），使 master 能「观测→再追加」形成 Claude Code 式循环。绝不在 `RunLocks` 锁内 await。

### W7c — 自主长循环（F1）
- 让 `autonomous` 自主模式真生效：master run 带**目标 + 退出条件**跑 `plan→dispatch→observe→adapt` 持久循环。**嫁接 AutoWork 的持久循环外壳**（`claim→inject→wait→finalize→idle-on-wake→repeat`）+ 复用 `resume_persisted_runs`（boot-resume）+ `reap_stalled_runs`（liveness）+ `RunLocks`。退出条件按 F1。
- 与 Phase 1a 可靠性联动：循环遇节点失败走「重试→中途修复回执→部分交付」而非停摆。

### W7d — 受控嵌套子委派（F2）
- 加 `delegation_depth` 字段（worker `extra` 传递 + 消费，超限拒绝）。
- 子委派**并入父 run 图**（W7a 的 `add_tasks` 复用，父 run 单画布）；或受管子 run + 活跃重唤起（按 F2）。
- 只读受限角色继续禁派发（`desktopGateway=false` 已天然禁网关工具）。

## 4. 迁移 / 数据模型（预估）

| 变更 | 归属 |
|---|---|
| `orch_run_tasks` 或 run 加 `delegation_depth`（或复用 worker extra 传递，不落库） | W7d |
| （可选）run 加自主循环 `goal`/`exit_condition`/`status=autonomous_running` 语义 | W7c |
| `RunOutcome` 加 `BatchProgress`（中途成功观测）| W7b |
| `add_tasks` 无需新表（复用 orch_run_tasks + deps）| W7a |

> 每加迁移 bump `db_lifecycle`（自动发现，无硬编码计数）；跨 crate 加字段必 `cargo check -p nomifun-app` 等全构造点（Phase 1 教训）。

## 5. 分期实施（分叉确认后各自 plan）

Phase 3 建议再拆两个可交付子相（**W7a/W7b 与 W7c/W7d 相对独立，可评估并行**——W7a 动 run_service/engine append，W7c 动自主循环外壳；若文件重叠则串行）：
- **Phase 3a — 动态派发底座**：W7a（单 run + add_tasks + 放宽 running-append）+ W7b（中途观测）。→ 交付「Claude Code 式随时派发/观测/再派发」。
- **Phase 3b — 自主 + 嵌套**：W7c（自主长循环）+ W7d（受控嵌套）。→ 交付「长周期自跑 + 受控深度委派」。

## 6. 风险

1. **运行时 append 与终态判定竞态**：放宽「running 时 append」后须确保追加节点在 `RunLocks` 内与 `finish_run` 原子不 strand（现有锁语义支持，但要专测）。
2. **单会话单 run 收敛的回归面**：现允许一会话多 run/重复 link；收敛要排查所有依赖「每次新建 run」的路径（含孤儿前门、伙伴域）。
3. **自主循环成本/失控**：长时间无人值守耗预算 + 可能绕圈；F1 的多退出条件 + 预算硬闸必须齐全；复用 IDMM/看门狗兜底。
4. **嵌套 × Phase1a 可靠性交互**：子委派节点失败的中途上报要上溯到正确的父 master，深度守卫防无限递归。
5. **中途观测 token 成本**：F4 节流参数需实测调。
6. **两套自愈相撞**：worker 是 yolo 会话可能被 IDMM 监管——与编排器重试/自主循环的交互须明确归属（Phase 1a 已列同风险，Phase 3 自主循环放大它）。

---

**下一步**：请审阅 §2 的五个分叉（F1–F5），确认或调整推荐默认；确认后我按 §5 拆 Phase 3a 先落地（动态派发底座），再 Phase 3b（自主+嵌套）。**在分叉确认前不动实现代码**——Phase 3 动引擎核心，方向须先定。

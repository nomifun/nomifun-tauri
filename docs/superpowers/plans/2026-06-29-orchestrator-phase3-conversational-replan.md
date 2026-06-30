# 智能编排 → Ultracode 增强 · Phase 3:会话驱动智能重调 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development。前端另需 frontend-design。**增强既有多 agent 编排引擎,严禁 IR/compile/typed-graph/用户手画图(已撤回)。** 承接 Phase 1(编排模式库)+ Phase 2(节点控制:重跑/微调/token + per-run 锁)。

**Goal:** 用户原始诉求——一个**全局意图框**,用户用自然语言表达意图,**主 agent 据意图 + 当前交付现状,智能判断保留/重分解/增删,重新调整编排**(不固定策略)。会话驱动 = 一序列「意图→主agent 调整编排」(一次性 LLM 调用,无常驻会话主管)。DAG + Phase-2 节点控制为实时结果视图。

**Architecture:** 新增「智能重调」能力:`RunService::adjust(run_id, intent)` → lead 模型(一次性)收到 {用户意图 + 当前 run 全态:每任务 id/title/spec/role/kind/status/output_summary 截断/deps} → 产出**调整后计划**(每节点 = KEEP(现有 task_id,保留其完成产出) 或 NEW(新任务),deps 引用 kept-id 或 new-index)→ 引擎**对账 reconcile**(保留 KEEP 任务的 status/output/conv/assignment,删除未保留的旧任务,新增 pending 新任务并路由,按调整计划重建 deps)→ 复用 Phase-2 per-run 锁 + 终态重激活 re-drive。主 agent prompt **明确赋权**:据意图与交付现状自行判断保留还是重做,不限死。新运行亦可由意图框创建(意图→初始计划)。

**Tech Stack:** Rust(nomifun-orchestrator plan/run_service/engine)+ React/Arco(意图框 UI)。

## Global Constraints
- 不引入 IR/compile/typed-graph/手画图。主 agent=一次性 lead 调用,无常驻会话(对齐用户「意图→编辑,无常驻会话」)。
- 复用 Phase-2 不变量:per-run 锁串行化(reconcile 与 loop 终止判定互斥,杜绝滞留);终态 run 重激活+engine.start(!is_running);reset 按 kind 保留模式策略;级联不越运行边界。
- 安全:reconcile 不破坏正在运行的 worker(actively-running 任务:reject 要求先暂停 OR 保留不动——选安全实现并文档);KEEP 的完成任务不重跑。
- 前端 typecheck0+build+check:i18n;icon-park 具名禁别名;`<div role=button>`;Arco useArcoMessage;无 any/ts-ignore;主题 token 必定义;UI 漂亮。
- 禁 cargo fmt;禁合并 main;提交前 git pull --rebase。

## File Structure
- `nomifun-orchestrator/src/plan.rs`(adjust prompt + 调整计划 schema/解析,支持 keep-id + new + 混合 deps;fail-soft)、`run_service.rs`(adjust + reconcile 对账)、`engine.rs`(reconcile 复用锁/重激活,新任务路由)、`api-types`(AdjustedPlan DTO + 请求体)+ TS 镜像、`routes.rs`(POST /runs/{id}/adjust + POST /runs/adhoc-from-intent 或复用)、ipcBridge。
- `ui/src/renderer/pages/orchestrator/`(意图框组件:新建运行 + 在运行中重调;复用 RunView/DagCanvas 展示结果)。

## 分期(3a 后端 reconcile 先行 → 3b 前端意图框)
### 3a 智能重调后端(adjust + reconcile)
- **plan.rs**:`ADJUST_SYSTEM` prompt——给 lead 当前 run 全态 + 用户意图,赋权它判断保留/重做;输出调整计划 JSON:`{"tasks":[ {"keep":"<task_id>"} | {"title","spec","role","kind","pattern_config?","depends_on":[<kept_id|new_index>]} ]}`。`parse_adjusted_plan`(fail-soft:坏 JSON → 不改 OR 错误返回,不崩;未知 kind→agent)。
- **run_service.rs `adjust(user, run_id, intent)`**:owner-scope;取当前 RunDetail;调 lead 产调整计划;**reconcile**:(1) KEEP 集=被保留的现有 task_id→原行 status/output/conv/tokens/assignment 不动;(2) 未保留的旧任务→删除(连 deps/assignment);(3) NEW 任务→建 pending 行 + 路由指派;(4) 按调整计划重建全部 deps(解析 kept-id/new-index→task_id);(5) 终态 run→running 重激活。**全程持 per-run 锁**(与 loop 终止互斥)。actively-running 任务的安全处理(reject 或保留,选定+文档)。
- **engine**:reconcile 走锁;路由复用现有 assignment 逻辑;re-drive 复用 rerun 的 !is_running→start。
- routes:`POST /api/orchestrator/runs/{id}/adjust` {intent} → adjust;（可选）新建运行从意图。ipcBridge `orchestrator.adjustRun`。
- 测试:意图→保留已完成+新增(KEEP 任务不重跑,output 保留;NEW 跑;run re-drive completed)；意图→重分解(主 agent 弃旧建新)；deps 重建正确;owner-scope;fail-soft 解析;无滞留(锁);agent/pattern 零回归。
- 提交 `feat(orchestrator): 会话驱动智能重调(主agent 据意图+现状判保留/重分解→reconnect 对账保留完成产出+新增+重建deps,复用锁/重激活)`。

### 3b 前端意图框
- 意图框组件(orchestrator 表面):自然语言输入 → adjustRun → DAG 更新(保留节点留存、新节点出现、run re-drive)。新建运行亦可由意图框(意图→初始计划,复用 createAdhoc 或 adjust-on-empty)。展示主 agent 的调整摘要(保留 N/新增 M/重做 K)。live 刷新。
- 提交 `feat(orchestrator/ui): 全局意图框(自然语言→主agent 智能重调编排)+ 调整摘要`。

## Self-Review / 风险
**覆盖:** 会话驱动→3a(后端智能 reconcile)+3b(意图框)。主 agent 判断保留/重做=用户核心诉求。
**不变量:** 无 IR;一次性主 agent 无常驻会话;复用 Phase-2 锁/重激活/按kind reset;KEEP 不重跑;reconcile 在锁内不滞留。
**风险:** ① reconcile 对账复杂(匹配 KEEP、删孤儿、重建混合 deps)——测覆盖;② 主 agent 输出质量(乱删完成工作)→prompt 约束 + 调整摘要让用户看到保留/删改 + 可用 Phase-2 重跑兜底;③ actively-running 任务安全(reject/保留);④ 新任务路由复用现有 router;⑤ 意图歧义→主 agent 一次性判断,用户可再发意图迭代。

## Execution Handoff
SDD:3a(后端 adjust+reconcile)→评审→3b(意图框 frontend-design)→评审。后端 sonnet;前端 frontend-design。复用 Phase-2 锁/重激活基建。禁 IR/cargo fmt/合并 main。

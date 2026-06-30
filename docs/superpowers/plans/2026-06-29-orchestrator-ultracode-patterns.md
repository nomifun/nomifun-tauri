# 智能编排 → Ultracode 增强 · Phase 1:编排模式库 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development。前端部分另需 frontend-design。**本阶段是对既有多 agent 编排引擎(crates/backend/nomifun-orchestrator,HEAD bad2de2a)的增强,不是重写。严禁引入 IR/compile()/类型化端口/用户手画图(那是已被撤回的错误方向,存档 tag archive/visual-workflows-rewrite)。**

**Goal:** 让主 agent 能像 Claude ultracode 那样编排**结构化多 agent 模式**(fan-out 扇出 / 对抗 verify / judge 评审团 / loop-until-dry / 综合 synthesis),在现有 task-DAG 引擎上以**任务 kind** 实现,主 agent 规划、引擎执行、现有 react-flow DAG 画布可视化(模式感知)。

**Architecture:** 给 `PlannedTask`/`RunTask` 加一个 `kind` 字段(默认 `agent` = 完全现状,零回归)+ 每 kind 的最小 config;教 `PLAN_SYSTEM` 何时用各模式;`engine.rs` 的 dispatch 按 kind 分支(agent=现状;pattern kind=模式执行);现有 DagCanvas 按 kind 渲染(分组/徽标)。模式的语义(N-skeptic 多数票/judge Borda/loop-until-dry)可**参考**存档 tag 里已验证的逻辑思路,但实现在 orch 引擎上(task+worker+dep),不搬其 Program/frame 机制。

**Tech Stack:** Rust(nomifun-orchestrator engine/plan/run_service + nomifun-db migration)+ React/react-flow(DagCanvas/TaskNode)。

## Global Constraints
- **不引入 IR/compile/typed-graph/用户手画图。** 节点=agent 任务(有 model/prompt/status),由主 agent 规划,用户观测+控制(重跑/换模型/微调 prompt),不手画类型化计算图。
- `kind` 默认 `agent`,既有 run/plan 零回归(旧计划无 kind→agent)。迁移 append-only(下一个号,注意 db_lifecycle forge 版本同步)。禁 cargo fmt;禁合并 main;提交前 git pull --rebase。
- 前端 typecheck0+build+check:i18n;icon-park 具名禁别名;`<div role=button>`;Arco useArcoMessage;无 any/ts-ignore;UI 漂亮;主题 token 必已定义。
- 现有引擎不变量勿破:bounded-parallel 调度/依赖严格/pause-resume 自门控/cancel 传播/boot-resume;LLM-primary+Router-veto 指派+lock-survives-replan;frozen fleet_snapshot。

## 模式定义(在 task-kind 上的最小实现)
- **agent**(默认):现状——一个 agent 执行一个任务。
- **synthesis**:一个任务,用 lead/指定模型把其**依赖任务的输出**智能综合(取代 engine.rs `aggregate_summary` 的机械拼接;该处已有 TODO)。单 worker,易。
- **fanout**:主 agent 把一步**扇出成 N 个并行兄弟子任务**(变体/分片);现有引擎已支持并行独立任务+downstream 依赖全部——主要是**教 planner 表达**+**可视化分组**+(可选)运行期按列表物化 N 子任务。
- **verify**:对某依赖输出跑 **N 个 skeptic agent**→多数票 pass/fail 裁决→gate 下游(失败可 skip 下游或触发 loop)。需 per-task 多 worker + 聚合(新)。
- **judge**:对候选(通常 fanout 的兄弟们)跑 **N 个 judge**→打分→择优。需多 worker + 聚合。
- **loop**:对某子任务**迭代重跑直到停止条件**(max_iter / dry / judge-approves)。需在无环引擎里加有界迭代(最大)。

## File Structure
- `nomifun-orchestrator/src/{plan.rs(教模式+schema 加 kind), engine.rs(dispatch 按 kind+模式执行+真 synthesis), run_service.rs, events.rs}`;`nomifun-api-types/src/orchestrator.rs`(PlannedTask/RunTask 加 kind+config)+ TS 镜像。
- `nomifun-db/migrations/0XX_task_kind.sql`(orch_run_tasks 加 kind+pattern_config)+ db_lifecycle 版本同步。
- `ui/src/renderer/pages/orchestrator/{RunDetail/DagCanvas,nodes/TaskNode,…}`(按 kind 渲染/分组/徽标)。

## 分期(1a 先交付→你验收→再 1b-1d)
### 1a 任务 kind 地基 + synthesis + fanout 规划(薄垂直切片,先交付验收)
- DB 迁移:orch_run_tasks 加 `kind TEXT NOT NULL DEFAULT 'agent'` + `pattern_config TEXT`(JSON,nullable)。DTO PlannedTask/RunTask 加 `kind`+`pattern_config`。
- plan.rs:PLAN_SYSTEM 教 `kind`(先 agent/synthesis/fanout),schema 加 kind;fallback 仍 agent。
- engine.rs:dispatch 按 kind 分支;`synthesis` kind=用 lead 模型把 deps 的 output_summary 智能综合(替换/增强 aggregate_summary,真 LLM synthesis);`fanout` 先按「planner 直接展开成 N 兄弟任务」表达(引擎无需新执行,纯并行独立任务)。agent kind 完全现状。
- 前端:TaskNode/DagCanvas 按 kind 显示徽标(synthesis/fanout 分组视觉);零回归 agent。
- 测试:plan 解析 kind;synthesis 任务真用 lead 综合 deps(engine 测);agent 默认零回归;旧计划无 kind→agent。门:orchestrator nextest + app build + 前端 typecheck0。**交付后停下,用户验收方向。**

### 1b verify(N skeptic 多数票 gate) · 1c judge(N judge 择优) · 1d loop(有界迭代)
- 各自:DTO/engine per-task 多 worker 执行 + 聚合 + 下游 gate/择优/迭代;planner 教该模式;UI 模式感知渲染(投票/分数/迭代轮次)。每个独立交付+评审+你验收。

## Self-Review / 风险
**覆盖:** ultracode 模式→1a-1d;节点控制(重跑/微调)= Phase 2;会话规划=Phase 3;打磨=Phase 4。
**不变量:** 不引入 IR/compile;kind 默认 agent 零回归;现有调度/指派/快照不变量保持;迁移 append-only。
**风险:** ① 不要滑回节点图编译器(纪律:task-kind on 既有引擎);② verify/judge 的 per-task 多 worker 是引擎新执行路径(需谨慎,参考存档语义但实现在 orch);③ loop 在无环引擎里需有界(max_iter 硬上限防失控);④ fanout 运行期物化(若超出 planner 静态展开)留 1b+;⑤ 主 agent 规划质量靠 prompt(模式滥用→提示词约束 + fallback agent)。

## Execution Handoff
SDD:1a(后端 kind 地基+synthesis+fanout 规划+前端徽标)→**交付验收**→1b verify→1c judge→1d loop。每子任务 fresh implementer + opus 对抗评审。后端 sonnet;前端 frontend-design。禁 cargo fmt;禁合并 main;禁 IR/compile。
